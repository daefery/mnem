use mnem::hook::cross_agent_delta;
use rusqlite::{Connection, params};

fn db(name: &str) -> Connection {
    let p = std::env::temp_dir().join(format!("mnem-delta-{}-{name}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    mnem::db::open(&p).unwrap()
}

fn seed(c: &Connection, sid: &str, kind: &str, text: &str) {
    c.execute("INSERT OR IGNORE INTO sessions(id, agent, native_id, project) VALUES (?1, 'pi', ?1, 'proj')", params![sid])
        .unwrap();
    c.execute(
        "INSERT INTO events(session_id, record_key, kind, text, turn, ts, source_path)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, '/live.jsonl')",
        params![
            sid,
            format!("{sid}:{kind}:{text}"),
            kind,
            text,
            mnem::db::now_ms()
        ],
    )
    .unwrap();
}

#[test]
fn every_session_is_eventually_shown_three_per_prompt() {
    let c = db("paging");
    c.execute(
        "INSERT INTO injections(session_id, watermark) VALUES ('claude:me', 0)",
        [],
    )
    .unwrap();
    for i in 0..12 {
        seed(
            &c,
            &format!("pi:s{i:02}"),
            "prompt",
            &format!("PROMPT{i:02}"),
        );
        seed(
            &c,
            &format!("pi:s{i:02}"),
            "assistant",
            &format!("ANSWER{i:02}"),
        );
    }
    let mut seen = std::collections::HashSet::new();
    for round in 0..4 {
        let d = cross_agent_delta(&c, "claude:me", "proj")
            .unwrap()
            .expect("pending work");
        for i in 0..12 {
            if d.contains(&format!("ANSWER{i:02}")) {
                assert!(seen.insert(i), "session {i} shown twice (round {round})");
            }
        }
    }
    assert_eq!(seen.len(), 12, "all sessions delivered across prompts");
    assert!(
        cross_agent_delta(&c, "claude:me", "proj")
            .unwrap()
            .is_none(),
        "nothing left"
    );
}

#[test]
fn error_only_work_is_reported() {
    let c = db("error");
    c.execute(
        "INSERT INTO injections(session_id, watermark) VALUES ('claude:me', 0)",
        [],
    )
    .unwrap();
    seed(
        &c,
        "pi:failed",
        "error",
        "Build failed: production regression",
    );
    let d = cross_agent_delta(&c, "claude:me", "proj")
        .unwrap()
        .expect("error shown");
    assert!(d.contains("production regression"), "{d}");
}

#[test]
fn imported_history_and_harness_prompts_are_not_news() {
    let c = db("noise");
    c.execute(
        "INSERT INTO injections(session_id, watermark) VALUES ('claude:me', 0)",
        [],
    )
    .unwrap();
    c.execute("INSERT INTO sessions(id, agent, native_id, project) VALUES ('pi:old', 'pi', 'old', 'proj')", []).unwrap();
    c.execute(
        "INSERT INTO events(session_id, record_key, kind, text, turn, ts) VALUES ('pi:old', 'cm:1', 'prompt', 'imported', 1, ?1)",
        params![mnem::db::now_ms()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO events(session_id, record_key, kind, text, turn, ts, source_path, label)
         VALUES ('pi:old', 'h', 'prompt', 'poll', 2, ?1, '/live.jsonl', 'harness')",
        params![mnem::db::now_ms()],
    )
    .unwrap();
    assert!(
        cross_agent_delta(&c, "claude:me", "proj")
            .unwrap()
            .is_none()
    );
}

#[test]
fn oversized_group_is_shown_whole() {
    let c = db("oversized");
    c.execute(
        "INSERT INTO injections(session_id, watermark) VALUES ('claude:me', 0)",
        [],
    )
    .unwrap();
    seed(&c, "pi:big", "prompt", &"p".repeat(3000));
    seed(&c, "pi:big", "assistant", &"a".repeat(3000));
    for i in 0..6 {
        c.execute(
            "INSERT INTO events(session_id, record_key, kind, path, turn, ts, source_path)
             VALUES ('pi:big', ?1, 'file_edit', ?2, 1, ?3, '/live.jsonl')",
            params![
                format!("f{i}"),
                format!("/x/{}{i}.rs", "n".repeat(240)),
                mnem::db::now_ms()
            ],
        )
        .unwrap();
    }
    seed(&c, "pi:big", "error", "CRITICAL-ERROR-OMITTED");
    let d = cross_agent_delta(&c, "claude:me", "proj").unwrap().unwrap();
    assert!(d.contains("CRITICAL-ERROR-OMITTED"), "{d}");
    assert!(d.len() <= 1600, "{}", d.len());
}

#[test]
fn recall_matches_prompt_once_per_session() {
    let c = db("recall");
    for (i, title) in [
        "Backup restore now stages the live database",
        "Viewer shows capture health",
        "Codex hooks need trust approval",
    ]
    .iter()
    .enumerate()
    {
        c.execute(
            "INSERT INTO memories(kind, type, title, project, origin, origin_id, created_at) VALUES ('observation', 'feature', ?1, 'proj', 'mnem', ?2, ?3)",
            params![title, format!("o{i}"), mnem::db::now_ms()],
        )
        .unwrap();
    }
    let prompt = "why does the backup restore refuse while the watch service is running";
    let r = mnem::recall::recall(&c, "claude:me", "proj", prompt)
        .unwrap()
        .expect("a match");
    assert!(r.contains("Backup restore"), "{r}");
    assert!(!r.contains("Viewer"), "{r}");
    assert!(
        mnem::recall::recall(&c, "claude:me", "proj", prompt)
            .unwrap()
            .is_none(),
        "not repeated"
    );
    assert!(
        mnem::recall::recall(&c, "claude:other", "proj", prompt)
            .unwrap()
            .is_some(),
        "other sessions still get it"
    );
    assert!(
        mnem::recall::recall(&c, "claude:x", "proj", "yes")
            .unwrap()
            .is_none(),
        "short prompts skip recall"
    );
    assert!(
        mnem::recall::recall(&c, "claude:x", "elsewhere", prompt)
            .unwrap()
            .is_none(),
        "project scoped"
    );
}

#[test]
fn half_written_line_is_not_an_alert() {
    let dir = std::env::temp_dir().join(format!("mnem-health-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let c = mnem::db::open(&dir.join("m.db")).unwrap();
    let f = dir.join("t.jsonl");
    std::fs::write(&f, "{\"a\":1}\n{\"half\":").unwrap();
    // Cursor sits after the first line; the rest is an unterminated record.
    c.execute(
        "INSERT INTO sources(path, agent, byte_offset, size_seen) VALUES (?1, 'pi', 8, 18)",
        params![f.to_string_lossy()],
    )
    .unwrap();
    // Age the file past the stuck threshold.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(mnem::health::stuck_files(&c), 0);
    // A complete unread line that old is stuck.
    std::fs::write(&f, "{\"a\":1}\n{\"b\":2}\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(mnem::health::stuck_files(&c), 1);
}

#[test]
fn export_labels_every_record() {
    let c = db("export");
    seed(&c, "pi:s", "prompt", "hello there");
    c.execute(
        "INSERT INTO memories(kind, type, title, project, origin, origin_id) VALUES ('observation', 'bugfix', 't', 'proj', 'mnem', 'x')",
        [],
    )
    .unwrap();
    let mut out = Vec::new();
    let n = mnem::eval::export(&c, &mut out, None).unwrap();
    let rows: Vec<serde_json::Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(n, rows.len());
    let kinds: Vec<&str> = rows.iter().map(|r| r["record"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["meta", "session", "event", "memory"]);
    assert_eq!(
        rows[3]["type"], "bugfix",
        "the memory's own type is preserved"
    );
}
