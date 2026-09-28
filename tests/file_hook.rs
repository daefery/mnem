//! The Claude Code PostToolUse hook: memories about a file when the agent first reads or
//! edits it in a session, with how the file changed since, and only once.

use rusqlite::params;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn hook(home: &Path, db: &Path, payload: &serde_json::Value) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mnem"))
        .env("MNEM_HOME", home)
        .env("MNEM_UI_PORT", "0")
        .arg("--db")
        .arg(db)
        .args(["hook", "claude", "file"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn first_touch_of_a_file_brings_its_memories_once() {
    let base = std::env::temp_dir().join(format!("mnem-file-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (repo, home) = (base.join("repo"), base.join("home"));
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(repo.join("src/quiet.rs"), "fn q() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "one"]);

    let t = mnem::files::resolve("src/a.rs", &repo).unwrap();
    let db = home.join("mnem.db");
    let c = mnem::db::open(&db).unwrap();
    // One memory from before an uncommitted edit, one from this very session.
    for (id, session, title) in [
        (1, "claude:earlier", "Retry loop in a() must stay bounded"),
        (2, "claude:now", "Working on a() right now"),
    ] {
        c.execute(
            "INSERT INTO memories(id, project, kind, type, title, files_modified, session_id, origin, origin_id, created_at)
             VALUES (?1, ?2, 'observation', 'decision', ?3, '[\"src/a.rs\"]', ?4, 'mnem', ?1, ?5)",
            params![id, t.project, title, session, mnem::db::now_ms() - 60_000],
        )
        .unwrap();
    }
    drop(c);
    std::fs::write(repo.join("src/a.rs"), "fn a() { loop {} }\n").unwrap();

    let touch = |file: &str| {
        serde_json::json!({
            "session_id": "now",
            "cwd": repo.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "tool_input": { "file_path": repo.join(file).to_string_lossy() }
        })
    };
    let out = hook(&home, &db, &touch("src/a.rs"));
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_else(|_| panic!("{out}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("src/a.rs") && ctx.contains("#1 "), "{ctx}");
    assert!(ctx.contains("Retry loop"), "{ctx}");
    assert!(ctx.contains("uncommitted edits"), "{ctx}");
    assert!(
        !ctx.contains("#2 "),
        "the session's own memory is left out: {ctx}"
    );

    // Once per file per session.
    assert_eq!(hook(&home, &db, &touch("src/a.rs")).trim(), "");
    // A file nobody remembers anything about: nothing.
    assert_eq!(hook(&home, &db, &touch("src/quiet.rs")).trim(), "");
    // Outside any repository: nothing, and no failure.
    let loose = serde_json::json!({
        "session_id": "now", "cwd": "/", "tool_name": "Read",
        "tool_input": { "file_path": "/etc/hostname" }
    });
    assert_eq!(hook(&home, &db, &loose).trim(), "");

    // pi's extension asks through `mnem file --touch`: same memories, plain text, once.
    let touch_as = |session: &str, file: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_mnem"))
            .env("MNEM_HOME", &home)
            .env("MNEM_UI_PORT", "0")
            .arg("--db")
            .arg(&db)
            .args(["file", file, "--touch", "--session", session, "--cwd"])
            .arg(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let touch_pi = |file: &str| touch_as("pi:p1", file);
    let first = touch_pi("src/a.rs");
    assert!(first.contains("#1 ") && first.contains("#2 "), "{first}");
    assert!(
        !first.trim_start().starts_with('{'),
        "plain text for pi: {first}"
    );
    assert_eq!(touch_pi("src/a.rs").trim(), "");
    assert_eq!(touch_pi("src/quiet.rs").trim(), "");
    let c = mnem::db::open(&db).unwrap();
    let (offers, runs): (i64, i64) = c
        .query_row(
            "SELECT (SELECT count(*) FROM offers WHERE session_id = 'pi:p1' AND source = 'file'),
                    (SELECT count(*) FROM hook_runs WHERE agent = 'pi' AND event = 'file')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((offers, runs), (2, 3));

    // Asked for explicitly with the session first: opening the file adds nothing more.
    let asked = Command::new(env!("CARGO_BIN_EXE_mnem"))
        .env("MNEM_HOME", &home)
        .arg("--db")
        .arg(&db)
        .args(["tool", "recall_file"])
        .arg(
            serde_json::json!({ "path": "src/a.rs", "cwd": repo.to_string_lossy(), "session": "pi:p2" })
                .to_string(),
        )
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&asked.stdout).contains("#1 "));
    assert_eq!(touch_as("pi:p2", "src/a.rs").trim(), "");
    // A memory this session was already shown (by prompt recall) is not shown again.
    c.execute(
        "INSERT INTO recall_seen(session_id, memory_id) VALUES ('pi:p3', 1)",
        [],
    )
    .unwrap();
    let third = touch_as("pi:p3", "src/a.rs");
    assert!(third.contains("#2 ") && !third.contains("#1 "), "{third}");
    // Parallel touches of one file (pi runs tool calls concurrently): shown once.
    let shown = std::thread::scope(|sc| {
        let runs: Vec<_> = (0..6)
            .map(|_| sc.spawn(|| touch_as("pi:p4", "src/a.rs")))
            .collect();
        runs.into_iter()
            .map(|r| r.join().unwrap())
            .filter(|out| !out.trim().is_empty())
            .count()
    });
    assert_eq!(shown, 1);
    let _ = std::fs::remove_dir_all(&base);
}
