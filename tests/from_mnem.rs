//! Upgrading from mnem, ravnori's name before 0.6.0: `rvn install` moves the data folder
//! and takes down everything only mnem names, and the user's memories, backups and own
//! settings come through. Runs the real binary in a scratch home.

use std::path::Path;
use std::process::Command;

fn rvn(home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rvn"))
        .args(args)
        .env("HOME", home)
        .env_remove("RAVNORI_HOME")
        .env_remove("MNEM_HOME")
        .env_remove("RAVNORI_CLAUDE_DIRS")
        .env_remove("MNEM_CLAUDE_DIRS")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rvn {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn install_moves_a_mnem_setup_to_ravnori() {
    let home = ravnori::TempDir::new("from-mnem");
    std::fs::create_dir_all(home.join("bin")).unwrap();
    // What mnem 0.5.0 left: its folder with a database (one distilled memory, one
    // forgotten), a backup and settings; its hooks next to one of the user's own; its MCP
    // server in Claude Code and Codex; its pi extension; its command.
    let old = home.join(".mnem");
    std::fs::create_dir_all(old.join("backups")).unwrap();
    {
        let c = ravnori::db::open(&old.join("mnem.db")).unwrap();
        // As an old build wrote them: origin 'mnem', schema one version back.
        c.execute_batch(
            "INSERT INTO memories(kind, title, origin, origin_id, project)
               VALUES ('observation', 'kept through the rename', 'mnem', 's@1-2#0', 'p');
             INSERT INTO forgotten(kind, key, at) VALUES ('memory', 'mnem:s@1-2#1', 1);
             PRAGMA user_version = 23;",
        )
        .unwrap();
    }
    std::fs::write(old.join("backups/mnem-20261001-000000.db"), b"x").unwrap();
    std::fs::write(
        old.join("config.json"),
        r#"{"distill": {"daily_calls": 42}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"theme": "dark", "hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/old/mnem hook claude stop"},
            {"type": "command", "command": "/usr/bin/my-own-hook"}]}]}}"#,
    )
    .unwrap();
    std::fs::write(
        home.join(".claude.json"),
        r#"{"mcpServers": {"mnem": {"command": "/old/mnem", "args": ["mcp"]}, "other": {"command": "o"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        "[mcp_servers.mnem]\ncommand = \"/old/mnem\"\n\n[mcp_servers.other]\ncommand = \"o\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".pi/agent/extensions/mnem")).unwrap();
    std::fs::write(home.join(".pi/agent/extensions/mnem/index.ts"), "// old").unwrap();
    std::fs::write(home.join("bin/mnem"), "#!/bin/sh\n").unwrap();

    // Before install: ravnori already reads the old folder, so nothing is lost in between.
    let doctor = rvn(&home, &["doctor"]);
    assert!(doctor.contains("distill:"), "{doctor}");

    let bin = home.join("bin/rvn");
    std::fs::copy(env!("CARGO_BIN_EXE_rvn"), &bin).unwrap();
    let out = rvn(
        &home,
        &[
            "install",
            "--only",
            "claude,codex,pi",
            "--bin",
            bin.to_str().unwrap(),
        ],
    );
    assert!(out.contains("moving from mnem"), "{out}");

    // Data: moved, the old path links to the new one, the database renamed.
    let new = home.join(".ravnori");
    assert!(
        new.join("ravnori.db").is_file(),
        "the database is renamed ravnori.db"
    );
    assert!(!new.join("mnem.db").exists());
    assert!(
        old.symlink_metadata().unwrap().file_type().is_symlink(),
        "~/.mnem should link to ~/.ravnori"
    );
    assert!(new.join("backups/mnem-20261001-000000.db").is_file());
    let cfg = std::fs::read_to_string(new.join("config.json")).unwrap();
    assert!(cfg.contains("\"daily_calls\": 42"), "settings kept: {cfg}");

    // Memories come through, under the new origin, and the forgotten one stays forgotten.
    let search = rvn(&home, &["tool", "search", r#"{"query":"rename"}"#]);
    assert!(search.contains("kept through the rename"), "{search}");
    let c = rusqlite::Connection::open(new.join("ravnori.db")).unwrap();
    let origins: Vec<String> = c
        .prepare("SELECT DISTINCT origin FROM memories")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(origins, ["ravnori"]);
    let key: String = c
        .query_row("SELECT key FROM forgotten WHERE kind = 'memory'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(key, "ravnori:s@1-2#1");

    // Hooks: mnem's replaced by ravnori's, the user's own kept.
    let s = std::fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    assert!(!s.contains("mnem hook"), "{s}");
    assert!(s.contains("rvn hook claude stop"), "{s}");
    assert!(s.contains("/usr/bin/my-own-hook"), "{s}");
    assert!(s.contains("\"theme\": \"dark\""), "{s}");

    // MCP: mnem's gone in both agents, ravnori's added in Codex (no `claude` here).
    let state = std::fs::read_to_string(home.join(".claude.json")).unwrap();
    assert!(!state.contains("\"mnem\""), "{state}");
    assert!(state.contains("\"other\""), "{state}");
    let codex = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(!codex.contains("mcp_servers.mnem"), "{codex}");
    assert!(codex.contains("mcp_servers.ravnori"), "{codex}");
    assert!(codex.contains("mcp_servers.other"), "{codex}");

    // pi: the old extension replaced by ravnori's.
    assert!(!home.join(".pi/agent/extensions/mnem").exists());
    assert!(home.join(".pi/agent/extensions/ravnori/index.ts").is_file());

    // The old command now runs rvn.
    assert_eq!(std::fs::read_link(home.join("bin/mnem")).unwrap(), bin);

    // A second install finds nothing left from mnem.
    let again = rvn(
        &home,
        &[
            "install",
            "--only",
            "claude,codex,pi",
            "--bin",
            bin.to_str().unwrap(),
        ],
    );
    assert!(!again.contains("moving from mnem"), "{again}");
}

#[test]
fn old_settings_names_still_work_for_one_release() {
    // MNEM_HOME, set by a 0.5.x setup, still points ravnori at its data.
    let home = ravnori::TempDir::new("from-mnem-env");
    let data = home.join("elsewhere");
    let out = Command::new(env!("CARGO_BIN_EXE_rvn"))
        .args(["doctor"])
        .env("HOME", &*home)
        .env_remove("RAVNORI_HOME")
        .env("MNEM_HOME", &data)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(data.join("ravnori.db").is_file(), "MNEM_HOME is used");
    assert!(!home.join(".ravnori").exists());
}
