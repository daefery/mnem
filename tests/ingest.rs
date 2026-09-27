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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(agent).join(file)
}

fn ingest(conn: &mut Connection, path: &Path, agent: Agent) -> ingest::CommitStats {
    let src = Source { path: path.to_path_buf(), agent };
    ingest::ingest_file(conn, &src, &mut Resolver::default()).unwrap()
}

fn rows(conn: &Connection) -> Vec<(String, String, String, bool)> {
    let mut s = conn
        .prepare("SELECT kind, coalesce(path, ''), coalesce(text, ''), is_error FROM events ORDER BY id")
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
    ingest(&mut conn, &fixture("claude", "session.jsonl"), Agent::Claude);
    assert_eq!(
        kinds(&conn),
        ["title", "prompt", "file_read", "command", "error", "file_edit", "assistant", "compaction", "recap"]
    );
    let r = rows(&conn);
    assert!(!r[1].2.contains("abcdefghijklmnop"), "secret leaked: {}", r[1].2);
    assert!(r[4].3, "error flag");
    assert_eq!(r[5].1, "/nonexistent/app/auth.rs");
    let (title, branch, project): (String, String, String) = conn
        .query_row("SELECT title, git_branch, project FROM sessions WHERE id = 'claude:s-claude'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((title.as_str(), branch.as_str(), project.as_str()), ("Fix login bug", "main", "/nonexistent/app"));
}

#[test]
fn codex_fixture_collapses_duplicate_envelopes() {
    let d = tmpdir("codex");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(&mut conn, &fixture("codex", "rollout.jsonl"), Agent::Codex);
    assert_eq!(kinds(&conn), ["prompt", "command", "error", "file_edit", "assistant"]);
    let project: String = conn
        .query_row("SELECT project FROM sessions WHERE id = 'codex:s-codex'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(project, "github.com/acme/svc", "falls back to session_meta remote when cwd is gone");
}

#[test]
fn pi_fixture() {
    let d = tmpdir("pi");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    ingest(&mut conn, &fixture("pi", "session.jsonl"), Agent::Pi);
    assert_eq!(kinds(&conn), ["prompt", "command", "file_edit", "error", "assistant", "compaction"]);
    let r = rows(&conn);
    assert!(r[3].2.contains("exited with code 2"));
}

#[test]
fn replay_from_zero_inserts_nothing() {
    let d = tmpdir("replay");
    let mut conn = mnem::db::open(&d.join("m.db")).unwrap();
    let f = fixture("claude", "session.jsonl");
    let first = ingest(&mut conn, &f, Agent::Claude).inserted;
    conn.execute("UPDATE sources SET byte_offset = 0, parser_state = NULL", []).unwrap();
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
    assert!(!kinds(&conn).contains(&"compaction".to_string()), "partial line must not be consumed");
    let quarantined: i64 = conn.query_row("SELECT count(*) FROM quarantine", [], |r| r.get(0)).unwrap();
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
    let src = Source { path: fixture("codex", "rollout.jsonl"), agent: Agent::Codex };
    // Both processes parse from the same (empty) cursor before either commits.
    let batch_a = ingest::parse(&src, ingest::load_cursor(&a, &src.path).unwrap()).unwrap().unwrap();
    let batch_b = ingest::parse(&src, ingest::load_cursor(&b, &src.path).unwrap()).unwrap().unwrap();
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
    let generation: i64 = conn.query_row("SELECT generation FROM sources", [], |r| r.get(0)).unwrap();
    assert_eq!(generation, 1);
    assert_eq!(kinds(&conn).len(), n);
}
