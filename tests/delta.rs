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
