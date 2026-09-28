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
    let _ = std::fs::remove_dir_all(&base);
}
