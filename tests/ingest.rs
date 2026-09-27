use mnem::ingest::{self, Source};
use mnem::model::Agent;
use mnem::project::Resolver;
use rusqlite::Connection;
use std::io::Write;
use std::path::{Path, PathBuf};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mnem-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fixture(agent: &str, file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(agent)
        .join(file)
}

fn ingest(conn: &mut Connection, path: &Path, agent: Agent) -> ingest::Outcome {
    let src = Source {
        path: path.to_path_buf(),
        agent,
    };
    ingest::ingest_file(conn, &src, &mut Resolver::default()).unwrap()
}

fn rows(conn: &Connection) -> Vec<(String, String, String, bool)> {
    let mut s = conn
        .prepare(
            "SELECT kind, coalesce(path, ''), coalesce(text, ''), is_error FROM events ORDER BY id",
        )
        .unwrap();
    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn kinds(conn: &Connection) -> Vec<String> {
    rows(conn).into_iter().map(|r| r.0).collect()
}

#[test]
fn claude_fixture() {
    let d = tmpdir("claude");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(
        &mut conn,
        &fixture("claude", "session.jsonl"),
        Agent::Claude,
    );
    assert_eq!(
        kinds(&conn),
        [
            "title",
            "prompt",
            "file_read",
            "command",
            "error",
            "file_edit",
            "assistant",
            "compaction",
            "recap"
        ]
    );
    let r = rows(&conn);
    assert!(
        !r[1].2.contains("abcdefghijklmnop"),
        "secret leaked: {}",
        r[1].2
    );
    assert!(r[4].3, "error flag");
    assert_eq!(r[5].1, "/nonexistent/app/auth.rs");
    let (title, branch, project): (String, String, String) = conn
        .query_row(
            "SELECT title, git_branch, project FROM sessions WHERE id = 'claude:s-claude'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (title.as_str(), branch.as_str(), project.as_str()),
        ("Fix login bug", "main", "/nonexistent/app")
    );
}

#[test]
fn codex_fixture_collapses_duplicate_envelopes() {
    let d = tmpdir("codex");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(&mut conn, &fixture("codex", "rollout.jsonl"), Agent::Codex);
    assert_eq!(
        kinds(&conn),
        ["prompt", "command", "error", "file_edit", "assistant"]
    );
    let project: String = conn
        .query_row(
            "SELECT project FROM sessions WHERE id = 'codex:s-codex'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        project, "github.com/acme/svc",
        "falls back to session_meta remote when cwd is gone"
    );
}

#[test]
fn pi_fixture() {
    let d = tmpdir("pi");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(&mut conn, &fixture("pi", "session.jsonl"), Agent::Pi);
    assert_eq!(
        kinds(&conn),
        [
            "prompt",
            "command",
            "file_edit",
            "error",
            "assistant",
            "compaction"
        ]
    );
    let r = rows(&conn);
    assert!(r[3].2.contains("exited with code 2"));
}

#[test]
fn replay_from_zero_inserts_nothing() {
    let d = tmpdir("replay");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = fixture("claude", "session.jsonl");
    let first = ingest(&mut conn, &f, Agent::Claude).inserted;
    conn.execute(
        "UPDATE sources SET byte_offset = 0, parser_state = NULL",
        [],
    )
    .unwrap();
    assert_eq!(ingest(&mut conn, &f, Agent::Claude).inserted, 0);
    assert_eq!(kinds(&conn).len(), first);
}

#[test]
fn partial_tail_waits_for_newline() {
    let d = tmpdir("partial");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let lines: Vec<String> = std::fs::read_to_string(fixture("pi", "session.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let f = d.join("s.jsonl");
    let (last, rest) = lines.split_last().unwrap();
    let (half_a, half_b) = last.split_at(last.len() / 2);
    std::fs::write(&f, format!("{}\n{half_a}", rest.join("\n"))).unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    assert!(
        !kinds(&conn).contains(&"compaction".to_string()),
        "partial line must not be consumed"
    );
    let quarantined: i64 = conn
        .query_row("SELECT count(*) FROM quarantine", [], |r| r.get(0))
        .unwrap();
    assert_eq!(quarantined, 0);
    let mut h = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    writeln!(h, "{half_b}").unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    assert_eq!(kinds(&conn).last().unwrap(), "compaction");
}

#[test]
fn concurrent_writer_loses_race_without_duplicates() {
    let d = tmpdir("race");
    let path = d.join("m.db");
    let mut a = mnem::db::open(&path).unwrap();
    let mut b = mnem::db::open(&path).unwrap();
    let src = Source {
        path: fixture("codex", "rollout.jsonl"),
        agent: Agent::Codex,
    };
    // Both processes parse from the same (empty) cursor before either commits.
    let batch_a = ingest::parse(&src, ingest::load_cursor(&a, &src.path).unwrap())
        .unwrap()
        .unwrap();
    let batch_b = ingest::parse(&src, ingest::load_cursor(&b, &src.path).unwrap())
        .unwrap()
        .unwrap();
    let ra = ingest::commit(&mut a, &batch_a, &mut Resolver::default()).unwrap();
    let rb = ingest::commit(&mut b, &batch_b, &mut Resolver::default()).unwrap();
    assert!(!ra.stale && rb.stale);
    assert_eq!(kinds(&a).len(), ra.inserted);
}

#[test]
fn rewritten_file_bumps_generation_and_dedupes() {
    let d = tmpdir("rewrite");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = d.join("s.jsonl");
    let original = std::fs::read_to_string(fixture("pi", "session.jsonl")).unwrap();
    std::fs::write(&f, &original).unwrap();
    let n = ingest(&mut conn, &f, Agent::Pi).inserted;
    // Simulate a rewrite: new header line, same records after it.
    let rewritten = original.replacen("\"version\":3", "\"version\":3,\"rewritten\":true", 1);
    std::fs::write(&f, rewritten).unwrap();
    assert_eq!(ingest(&mut conn, &f, Agent::Pi).inserted, 0);
    let generation: i64 = conn
        .query_row("SELECT generation FROM sources", [], |r| r.get(0))
        .unwrap();
    assert_eq!(generation, 1);
    assert_eq!(kinds(&conn).len(), n);
}

// Regression tests from council round 2 (code review).

#[test]
fn same_length_rewrite_with_same_header_is_detected() {
    let d = tmpdir("samelen");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = d.join("s.jsonl");
    let original = std::fs::read_to_string(fixture("pi", "session.jsonl")).unwrap();
    std::fs::write(&f, &original).unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    // Same first line, same byte length, different content in the consumed prefix.
    let changed = original.replace("update the header", "update the footer");
    assert_eq!(changed.len(), original.len());
    std::fs::write(&f, changed).unwrap();
    assert_eq!(ingest(&mut conn, &f, Agent::Pi).inserted, 1);
    let texts: Vec<String> = rows(&conn).into_iter().map(|r| r.2).collect();
    assert!(
        texts.contains(&"update the footer".to_string()),
        "{texts:?}"
    );
    assert!(
        !texts.contains(&"update the header".to_string()),
        "stale text kept: {texts:?}"
    );
}

#[test]
fn quarantine_is_redacted() {
    let d = tmpdir("quarantine");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = d.join("s.jsonl");
    std::fs::write(
        &f,
        "{\"type\":\"session\",\"id\":\"x\"}\n{broken token=verysecret123456789\n",
    )
    .unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    let line: String = conn
        .query_row("SELECT line FROM quarantine", [], |r| r.get(0))
        .unwrap();
    assert!(!line.contains("verysecret"), "{line}");
}

#[test]
fn incompatible_parser_state_replays_from_zero() {
    let d = tmpdir("state");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = d.join("s.jsonl");
    let lines: Vec<&str> = include_str!("fixtures/pi/session.jsonl").lines().collect();
    std::fs::write(&f, format!("{}\n", lines[..2].join("\n"))).unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    conn.execute("UPDATE sources SET parser_state = '{}'", [])
        .unwrap();
    std::fs::write(&f, format!("{}\n", lines.join("\n"))).unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    let sessions: Vec<String> = conn
        .prepare("SELECT DISTINCT session_id FROM events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        sessions,
        ["pi:s-pi"],
        "events must stay attributed to the real session"
    );
}

#[test]
fn losing_writer_retries_and_catches_up() {
    let d = tmpdir("overlap");
    let path = d.join("m.db");
    let mut a = mnem::db::open(&path).unwrap();
    let mut b = mnem::db::open(&path).unwrap();
    let f = d.join("s.jsonl");
    let lines: Vec<&str> = include_str!("fixtures/pi/session.jsonl").lines().collect();
    std::fs::write(&f, format!("{}\n", lines[..2].join("\n"))).unwrap();
    let src = Source {
        path: f.clone(),
        agent: Agent::Pi,
    };
    let small = ingest::parse(&src, None).unwrap().unwrap();
    std::fs::write(&f, format!("{}\n", lines.join("\n"))).unwrap();
    let large = ingest::parse(&src, None).unwrap().unwrap();
    ingest::commit(&mut a, &small, &mut Resolver::default()).unwrap();
    assert!(
        ingest::commit(&mut b, &large, &mut Resolver::default())
            .unwrap()
            .stale
    );
    let o = ingest::ingest_file(&mut b, &src, &mut Resolver::default()).unwrap();
    assert_eq!(o.status, ingest::Status::CaughtUp);
    assert_eq!(
        kinds(&a).last().unwrap(),
        "compaction",
        "suffix from the losing batch is not lost"
    );
}

#[test]
fn restored_file_clears_missing_flag() {
    let d = tmpdir("missing");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let src = Source {
        path: fixture("pi", "session.jsonl"),
        agent: Agent::Pi,
    };
    ingest(&mut conn, &src.path, Agent::Pi);
    assert_eq!(ingest::mark_missing(&conn, &[]).unwrap(), 1);
    ingest::mark_missing(&conn, std::slice::from_ref(&src)).unwrap();
    let missing: Option<i64> = conn
        .query_row("SELECT missing_since FROM sources", [], |r| r.get(0))
        .unwrap();
    assert_eq!(missing, None);
}

#[test]
fn subagent_events_carry_thread_and_merge_into_parent_session() {
    let d = tmpdir("subagent");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let sub = d.join("s-claude/subagents");
    std::fs::create_dir_all(&sub).unwrap();
    let f = sub.join("agent-abc.jsonl");
    std::fs::write(
        &f,
        concat!(
            r#"{"type":"assistant","uuid":"x1","sessionId":"s-claude","isSidechain":true,"message":{"content":[{"type":"tool_use","id":"t9","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n"
        ),
    )
    .unwrap();
    ingest(&mut conn, &f, Agent::Claude);
    let (sid, thread, tool, raw): (String, String, String, String) = conn
        .query_row(
            "SELECT session_id, thread, tool, tool_raw FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (sid.as_str(), thread.as_str(), tool.as_str(), raw.as_str()),
        ("claude:s-claude", "agent-abc", "shell", "Bash")
    );
}

#[test]
fn nested_workflow_subagent_gets_thread() {
    let d = tmpdir("nested");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let dir = d.join("s-claude/subagents/workflows/wf_1");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("agent-xyz.jsonl");
    std::fs::write(
        &f,
        concat!(
            r#"{"type":"assistant","uuid":"y1","sessionId":"s-claude","isSidechain":true,"message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/x/a.rs"}}]}}"#,
            "\n"
        ),
    )
    .unwrap();
    ingest(&mut conn, &f, Agent::Claude);
    let thread: String = conn
        .query_row("SELECT thread FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(thread, "agent-xyz");
}

#[test]
fn human_repeats_are_turns_but_harness_polling_collapses() {
    let d = tmpdir("repeats");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = d.join("s.jsonl");
    let msg = |id: &str, t: &str| {
        format!(
            r#"{{"type":"message","id":"{id}","message":{{"role":"user","content":[{{"type":"text","text":"{t}"}}]}}}}"#
        )
    };
    let lines = [
        r#"{"type":"session","id":"s","cwd":"/x"}"#.to_string(),
        msg("1", "yes"),
        msg("2", "yes"),
        msg("3", "⁣poll A"),
        msg("4", "⁣poll B"),
        msg("5", "⁣poll A"),
    ];
    std::fs::write(&f, lines.join("\n") + "\n").unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    let prompts: Vec<(String, Option<String>)> = conn
        .prepare("SELECT text, label FROM events WHERE kind = 'prompt' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let texts: Vec<&str> = prompts.iter().map(|p| p.0.as_str()).collect();
    assert_eq!(texts, ["yes", "yes", "poll A", "poll B"]);
    assert_eq!(prompts[2].1.as_deref(), Some("harness"));
}

#[test]
fn final_answer_is_last_text_of_the_turn() {
    let d = tmpdir("final");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(
        &mut conn,
        &fixture("claude", "session.jsonl"),
        Agent::Claude,
    );
    let a = mnem::context::final_answer(&conn, "claude:s-claude", 1).unwrap();
    assert_eq!(
        a.as_deref(),
        Some("Fixed: token expiry used < instead of <=.")
    );
}

#[test]
fn import_summary_gets_fresh_id_when_reserved_id_is_taken() {
    let d = tmpdir("import-ids");
    // Minimal claude-mem database with one session and one summary (#7).
    let src = d.join("cm.db");
    let cm = Connection::open(&src).unwrap();
    cm.execute_batch(
        "CREATE TABLE sdk_sessions(memory_session_id TEXT, content_session_id TEXT, platform_source TEXT, project TEXT,
           started_at_epoch INTEGER, completed_at_epoch INTEGER, custom_title TEXT);
         CREATE TABLE observations(id INTEGER, memory_session_id TEXT, project TEXT, type TEXT, title TEXT, subtitle TEXT,
           narrative TEXT, facts TEXT, concepts TEXT, files_read TEXT, files_modified TEXT, generated_by_model TEXT, created_at_epoch INTEGER);
         CREATE TABLE session_summaries(id INTEGER, memory_session_id TEXT, project TEXT, request TEXT, investigated TEXT, learned TEXT,
           completed TEXT, next_steps TEXT, notes TEXT, files_read TEXT, files_edited TEXT, created_at_epoch INTEGER);
         CREATE TABLE user_prompts(id INTEGER, content_session_id TEXT, prompt_number INTEGER, prompt_text TEXT, created_at_epoch INTEGER);
         INSERT INTO sdk_sessions VALUES ('m1', 'c1', 'claude', 'p', 1, 2, NULL);
         INSERT INTO session_summaries VALUES (7, 'm1', 'p', 'Ship it', 'logs', '', '', '', '', NULL, NULL, 5);",
    )
    .unwrap();
    drop(cm);
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    // A distilled mnem memory already occupies 1,000,007.
    conn.execute(
        "INSERT INTO memories(id, kind, title, origin, origin_id) VALUES (1000007, 'observation', 'mine', 'mnem', 'x')",
        [],
    )
    .unwrap();
    unsafe { std::env::set_var("MNEM_HOME", &d) };
    let s = mnem::import::claude_mem(&mut conn, &src).unwrap();
    assert_eq!(s.summaries, 1);
    let title: String = conn
        .query_row(
            "SELECT title FROM memories WHERE origin_id = 'sum:7'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(title, "Ship it");
    let mine: String = conn
        .query_row("SELECT title FROM memories WHERE id = 1000007", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(mine, "mine");
}

#[test]
fn distilled_memory_links_only_to_events_it_was_shown() {
    // cited_ids is private; exercise it through the public surface instead: a fake LLM
    // reply is stored via the same path in distill's unit tests. Here we check the
    // evidence table and its rendering in get_observations.
    let d = tmpdir("evidence");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(
        &mut conn,
        &fixture("claude", "session.jsonl"),
        Agent::Claude,
    );
    let prompt_id: i64 = conn
        .query_row("SELECT id FROM events WHERE kind = 'prompt'", [], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO memories(id, session_id, kind, type, title, origin, origin_id) VALUES (2000000, 'claude:s-claude', 'observation', 'bugfix', 'Login expiry fixed', 'mnem', 'x')",
        [],
    )
    .unwrap();
    let t: String = conn
        .query_row("SELECT text FROM events WHERE id = ?1", [prompt_id], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO memory_evidence(memory_id, event_id, event_hash) VALUES (2000000, ?1, ?2)",
        rusqlite::params![prompt_id, mnem::text::hash(&t)],
    )
    .unwrap();
    let out = mnem::mcp::call(
        &conn,
        "get_observations",
        &serde_json::json!({ "ids": [2000000] }),
    )
    .unwrap();
    assert!(out.contains(&format!("E{prompt_id} [prompt]")), "{out}");
    assert!(!out.contains("changed since"), "{out}");
    conn.execute(
        "UPDATE events SET text = 'rewritten' WHERE id = ?1",
        [prompt_id],
    )
    .unwrap();
    let out = mnem::mcp::call(
        &conn,
        "get_observations",
        &serde_json::json!({ "ids": [2000000] }),
    )
    .unwrap();
    assert!(out.contains("changed since"), "{out}");
}

#[test]
fn forgotten_data_never_comes_back() {
    let d = tmpdir("forget");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = fixture("pi", "session.jsonl");
    ingest(&mut conn, &f, Agent::Pi);
    let prompt: i64 = conn
        .query_row("SELECT id FROM events WHERE kind = 'prompt'", [], |r| {
            r.get(0)
        })
        .unwrap();
    // Forget one event, then replay the whole transcript from zero.
    mnem::forget::forget(&mut conn, &[format!("E{prompt}")], None, None).unwrap();
    conn.execute(
        "UPDATE sources SET byte_offset = 0, parser_state = NULL",
        [],
    )
    .unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE kind = 'prompt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "forgotten event resurrected by replay");
    // Forget the whole session: replay stores nothing for it.
    mnem::forget::forget(&mut conn, &[], Some("pi:s-pi"), None).unwrap();
    conn.execute(
        "UPDATE sources SET byte_offset = 0, parser_state = NULL",
        [],
    )
    .unwrap();
    ingest(&mut conn, &f, Agent::Pi);
    let n: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "forgotten session resurrected by replay");
}

#[test]
fn pinned_facts_open_the_context() {
    let d = tmpdir("pin");
    let conn = mnem::db::open(&d.join("m.db")).unwrap();
    mnem::forget::remember(
        &conn,
        "Deploys go through the staging branch first",
        Some("proj"),
    )
    .unwrap();
    mnem::forget::remember(&conn, "Reply in English", None).unwrap();
    let ctx = mnem::context::build(
        &conn,
        &mnem::context::Options {
            project: "proj",
            current: None,
            budget_chars: 8000,
            sessions: 5,
            turns: 3,
            observations: 30,
        },
    )
    .unwrap();
    assert!(ctx.contains("## Pinned"), "{ctx}");
    assert!(
        ctx.contains("staging branch") && ctx.contains("Reply in English"),
        "{ctx}"
    );
    let other = mnem::context::build(
        &conn,
        &mnem::context::Options {
            project: "other",
            current: None,
            budget_chars: 8000,
            sessions: 5,
            turns: 3,
            observations: 30,
        },
    )
    .unwrap();
    assert!(
        !other.contains("staging branch") && other.contains("Reply in English"),
        "{other}"
    );
}
