//! The daily request limit holds for all background distillation: the per-turn Stop
//! hook stops at `distill.daily_calls` like the watcher's backfill does, while a person
//! running `mnem distill` by hand is not limited. Runs the real binary in a scratch home
//! with a fake `claude` that counts its calls.

use std::path::Path;
use std::process::Command;

fn scratch(name: &str) -> mnem::TempDir {
    let d = mnem::TempDir::new(&format!("limit-{name}"));
    std::fs::create_dir_all(d.join("bin")).unwrap();
    std::fs::create_dir_all(d.join(".mnem")).unwrap();
    d
}

fn mnem(home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_mnem"))
        .args(args)
        .env("HOME", home)
        .env("MNEM_HOME", home.join(".mnem"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        )
        .env_remove("MNEM_CLAUDE_DIRS")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mnem {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A `claude` that answers every request with no memories and counts the requests.
fn fake_claude(home: &Path) {
    let p = home.join("bin/claude");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.284 (Claude Code)'; exit 0;; esac\ncat >/dev/null\necho x >> {}\necho '{{\"type\":\"result\",\"is_error\":false,\"result\":\"{{\\\"observations\\\":[],\\\"summary\\\":null}}\"}}'\n",
            home.join("calls").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn calls(home: &Path) -> usize {
    std::fs::read_to_string(home.join("calls"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// One idle session with enough work for a digest, and `spent` requests already
/// recorded in the last 24 hours.
fn seed(home: &Path, spent: usize) {
    let conn = mnem::db::open(&home.join(".mnem/mnem.db")).unwrap();
    let now = mnem::db::now_ms();
    conn.execute(
        "INSERT INTO sessions(id, agent, native_id, project, last_event_at) VALUES ('claude:s1', 'claude', 's1', 'p', ?1)",
        [now - 600_000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('claude:s1', 'k1', ?1, 'prompt', ?2)",
        rusqlite::params![now - 600_000, "fix the retry loop that hammers the rates API on 429 ".repeat(20)],
    )
    .unwrap();
    for _ in 0..spent {
        conn.execute(
            "INSERT INTO distill_calls(at, source, model, ok, requests) VALUES (?1, 'watch', 'm', 1, 1)",
            [now - 60_000],
        )
        .unwrap();
    }
}

#[test]
fn background_distillation_stops_at_the_daily_limit_and_manual_does_not() {
    let home = scratch("spent");
    fake_claude(&home);
    std::fs::write(
        home.join(".mnem/config.json"),
        r#"{"distill": {"provider": "claude-cli", "daily_calls": 5}}"#,
    )
    .unwrap();
    seed(&home, 5);

    // What the Stop hook runs: the limit is spent, so nothing is sent and nothing lost.
    let stop = [
        "distill",
        "--session",
        "claude:s1",
        "--active",
        "--quiet",
        "--background",
        "--limit",
        "1",
    ];
    mnem(&home, &stop);
    assert_eq!(calls(&home), 0, "the Stop hook went past the daily limit");
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        doctor.contains("daily limit reached: 1 session(s) wait"),
        "doctor does not say the limit holds work back:\n{doctor}"
    );

    // The same session by hand: a person asked, so it is sent.
    mnem(
        &home,
        &["distill", "--session", "claude:s1", "--active", "--quiet"],
    );
    assert_eq!(calls(&home), 1, "a manual distill should not be limited");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_stop_hook_distils_while_the_limit_has_room() {
    let home = scratch("room");
    fake_claude(&home);
    std::fs::write(
        home.join(".mnem/config.json"),
        r#"{"distill": {"provider": "claude-cli", "daily_calls": 5}}"#,
    )
    .unwrap();
    seed(&home, 4);
    mnem(
        &home,
        &[
            "distill",
            "--session",
            "claude:s1",
            "--active",
            "--quiet",
            "--background",
            "--limit",
            "1",
        ],
    );
    assert_eq!(calls(&home), 1, "one request was left in the limit");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn daily_calls_zero_turns_off_backfill_not_memories() {
    let home = scratch("zero");
    fake_claude(&home);
    std::fs::write(
        home.join(".mnem/config.json"),
        r#"{"distill": {"provider": "claude-cli", "daily_calls": 0}}"#,
    )
    .unwrap();
    seed(&home, 3);
    mnem(
        &home,
        &[
            "distill",
            "--session",
            "claude:s1",
            "--active",
            "--quiet",
            "--background",
            "--limit",
            "1",
        ],
    );
    assert_eq!(
        calls(&home),
        1,
        "daily_calls = 0 should only turn backfill off"
    );
    let _ = std::fs::remove_dir_all(&home);
}
