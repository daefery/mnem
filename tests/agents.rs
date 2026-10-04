//! Connecting agents in any order: mnem installed before an agent, after it, or with
//! the agent's own command missing. Runs the real binary in a scratch home folder.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mnem-agents-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `mnem <args>` with HOME in the scratch folder and a PATH holding only `bin`
/// (fake agent commands) and the system's basics.
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

/// A fake agent command in the scratch PATH. `claude` records its arguments and, for
/// `mcp add`, writes what the real one writes.
fn fake_command(home: &Path, name: &str) {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = if name == "claude" {
        format!(
            "#!/bin/sh\necho \"$@\" >> {log}\nif [ \"$1 $2\" = \"mcp add\" ]; then printf '{{\"mcpServers\":{{\"mnem\":{{\"type\":\"stdio\",\"command\":\"x\",\"args\":[\"mcp\"]}}}}}}' > {state}; fi\n",
            log = home.join("claude-calls.log").display(),
            state = home.join(".claude.json").display()
        )
    } else {
        "#!/bin/sh\nexit 0\n".into()
    };
    let p = bin.join(name);
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The agent's line in the report's "agents" section.
fn line<'a>(report: &'a str, agent: &str) -> &'a str {
    let section = report.split("\nagents\n").nth(1).unwrap_or(report);
    section
        .lines()
        .find(|l| l.trim_start().starts_with(agent))
        .unwrap_or_else(|| panic!("no {agent} line in:\n{report}"))
}

#[test]
fn mnem_first_then_agents_later() {
    let home = scratch("first");
    // Nothing installed yet: mnem still writes every agent's part.
    let out = mnem(&home, &["install"]);
    assert!(out.contains("Claude Code ("), "{out}");
    assert!(home.join(".claude/settings.json").is_file());
    assert!(home.join(".codex/hooks.json").is_file());
    assert!(home.join(".pi/agent/extensions/mnem/index.ts").is_file());
    // Without the claude command, the tools are registered directly.
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(state["mcpServers"]["mnem"]["args"][0], "mcp");
    let doctor = mnem(&home, &["doctor"]);
    for agent in ["Claude Code", "Codex", "pi"] {
        assert!(
            line(&doctor, agent).contains("not installed yet; mnem is ready"),
            "{doctor}"
        );
    }

    // The agents arrive later: Claude Code and pi are connected with no further step;
    // Codex asks the user to trust the hooks.
    fake_command(&home, "claude");
    fake_command(&home, "codex");
    fake_command(&home, "pi");
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        line(&doctor, "Claude Code").contains("connected"),
        "{doctor}"
    );
    assert!(line(&doctor, "pi").contains("connected"), "{doctor}");
    assert!(line(&doctor, "Codex").contains("not trusted"), "{doctor}");
    assert!(doctor.contains("/hooks"), "{doctor}");

    // The user trusts them in Codex (it writes trust entries); now connected.
    let hooks = home.join(".codex/hooks.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    let mut trust = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    for (event, groups) in doc["hooks"].as_object().unwrap() {
        let snake: String = event
            .chars()
            .enumerate()
            .flat_map(|(i, c)| {
                if c.is_ascii_uppercase() && i > 0 {
                    vec!['_', c.to_ascii_lowercase()]
                } else {
                    vec![c.to_ascii_lowercase()]
                }
            })
            .collect();
        for gi in 0..groups.as_array().unwrap().len() {
            trust.push_str(&format!(
                "\n[hooks.state.\"{}:{snake}:{gi}:0\"]\ntrusted_hash = \"sha256:x\"\n",
                hooks.display()
            ));
        }
    }
    std::fs::write(home.join(".codex/config.toml"), trust).unwrap();
    let doctor = mnem(&home, &["doctor"]);
    assert!(line(&doctor, "Codex").contains("connected"), "{doctor}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_agent_whose_settings_lost_mnem_is_reported_and_reconnected() {
    let home = scratch("lost");
    fake_command(&home, "claude");
    fake_command(&home, "pi");
    mnem(&home, &["install", "--only", "claude,pi"]);
    // The agent was reinstalled over its settings: mnem's hooks are gone.
    std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        line(&doctor, "Claude Code").contains("NOT CONNECTED (memory hooks)"),
        "{doctor}"
    );
    assert!(doctor.contains("mnem install --only claude"), "{doctor}");
    assert!(doctor.contains("status: ATTENTION"), "{doctor}");
    // Connecting it again, as the viewer's button does.
    mnem(&home, &["install", "--only", "claude"]);
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        line(&doctor, "Claude Code").contains("connected"),
        "{doctor}"
    );
    // The claude command was used for the tools when it existed.
    let calls = std::fs::read_to_string(home.join("claude-calls.log")).unwrap();
    assert!(calls.contains("mcp add --scope user mnem"), "{calls}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn uninstall_removes_only_what_mnem_created() {
    let home = scratch("uninstall");
    mnem(&home, &["install", "--only", "claude"]);
    let state = std::fs::read_to_string(home.join(".claude.json")).unwrap();
    assert!(state.contains("\"mnem\""));
    mnem(&home, &["uninstall"]);
    assert!(
        !home.join(".claude.json").exists(),
        "mnem's own file is removed"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Claude Code's own state file is never edited by mnem: without the `claude` command it
/// says what to run instead.
#[test]
fn claude_code_state_is_left_to_claude_code() {
    let home = scratch("state");
    let theirs = r#"{"numStartups": 7, "projects": {"/w": {"allowedTools": []}}}"#;
    std::fs::write(home.join(".claude.json"), theirs).unwrap();
    let out = mnem(&home, &["install", "--only", "claude"]);
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        theirs
    );
    assert!(out.contains("claude mcp add --scope user mnem"), "{out}");
    // Claude Code's own state means it is installed: reported, with what is missing.
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        line(&doctor, "Claude Code").contains("NOT CONNECTED (tools)"),
        "{doctor}"
    );
    mnem(&home, &["uninstall"]);
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        theirs
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A partial set of hooks, or tools pointing at a binary that is gone, is not connected.
#[test]
fn half_wired_agents_are_not_reported_connected() {
    let home = scratch("half");
    fake_command(&home, "codex");
    fake_command(&home, "pi");
    mnem(&home, &["install", "--only", "codex,pi"]);
    // One hook gone.
    let hooks = home.join(".codex/hooks.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    doc["hooks"].as_object_mut().unwrap().remove("SessionStart");
    std::fs::write(&hooks, doc.to_string()).unwrap();
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        line(&doctor, "Codex").contains("NOT CONNECTED (memory hooks)"),
        "{doctor}"
    );
    // Reconnect, then point the tools at a binary that no longer exists.
    mnem(&home, &["install", "--only", "codex"]);
    let cfg = home.join(".codex/config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let moved = text
        .lines()
        .map(|l| {
            if l.starts_with("command = ") {
                "command = \"/gone/mnem\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&cfg, moved).unwrap();
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        line(&doctor, "Codex").contains("no longer exists"),
        "{doctor}"
    );
    // Reinstalling corrects the command in place.
    mnem(&home, &["install", "--only", "codex"]);
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(!text.contains("/gone/mnem"), "{text}");
    assert_eq!(text.matches("[mcp_servers.mnem]").count(), 1, "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Re-running install changes nothing that is already right: no rewrite, no backup.
#[test]
fn a_repeat_install_writes_nothing() {
    let home = scratch("repeat");
    mnem(&home, &["install"]);
    let files = |home: &Path| -> Vec<String> {
        let mut v: Vec<String> = walk(home)
            .into_iter()
            .filter(|p| p.to_string_lossy().contains(".bak-mnem-"))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let before = files(&home);
    let out = mnem(&home, &["install"]);
    assert_eq!(files(&home), before, "no new backups:\n{out}");
    let _ = std::fs::remove_dir_all(&home);
}

fn walk(d: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// A fresh install with no settings captures but makes no memories: install and doctor
/// both say so, and a configured key silences it.
#[test]
fn without_a_model_install_and_doctor_say_no_memories_are_made() {
    let home = scratch("no-model");
    let warning = "no memories are being made";
    let report = mnem(&home, &["install", "--only", "pi"]);
    assert!(report.contains(warning), "install report:\n{report}");
    let doctor = mnem(&home, &["doctor"]);
    assert!(doctor.contains(warning), "doctor:\n{doctor}");

    std::fs::write(
        home.join(".mnem/config.json"),
        r#"{"distill": {"api_key_env": "MNEM_TEST_KEY"}}"#,
    )
    .unwrap();
    let doctor = mnem(&home, &["doctor"]);
    assert!(!doctor.contains(warning), "doctor with a key:\n{doctor}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A fake `claude` that records how it was called and its stdin, then answers like the
/// real one does with `--output-format json`.
fn fake_claude_distiller(home: &Path) {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = home.join("claude-distill.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$@" > {args}
cat > {stdin}
pwd > {cwd}
cat <<'JSON'
{{"type":"result","is_error":false,"result":"```json\n{{\"observations\": [{{\"type\": \"discovery\", \"title\": \"Fake memory from the CLI provider\", \"narrative\": \"n\", \"facts\": [\"f\"], \"evidence\": []}}], \"summary\": null}}\n```"}}
JSON
"#,
        args = log.with_extension("args").display(),
        stdin = log.with_extension("stdin").display(),
        cwd = log.with_extension("cwd").display()
    );
    let p = bin.join("claude");
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// With Claude Code installed and nothing configured, install sets distillation to its
/// command line (sonnet, 100 a day), doctor is quiet, and a distillation run goes through
/// it isolated: no tools, no user settings, no saved session, an empty directory. An
/// existing setup is never overwritten.
#[test]
fn install_uses_claude_code_for_distillation_when_nothing_is_configured() {
    let home = scratch("cli-provider");
    fake_claude_distiller(&home);
    let report = mnem(&home, &["install", "--only", "pi"]);
    assert!(
        report.contains("distillation: using `claude`"),
        "install report:\n{report}"
    );
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".mnem/config.json")).unwrap())
            .unwrap();
    assert_eq!(cfg["distill"]["provider"], "claude-cli");
    assert_eq!(cfg["distill"]["daily_calls"], 100);
    let doctor = mnem(&home, &["doctor"]);
    assert!(
        !doctor.contains("no memories are being made"),
        "doctor:\n{doctor}"
    );

    // One session with enough to distil, then a distillation run through the fake.
    let c = mnem::db::open(&home.join(".mnem/mnem.db")).unwrap();
    c.execute("INSERT INTO sessions(id, agent, native_id, project, last_event_at) VALUES ('pi:s1','pi','s1','proj',1)", [])
        .unwrap();
    for (i, (kind, text)) in [
        ("prompt", "fix the retry loop so it backs off on 429"),
        (
            "assistant",
            &"Changed src/net.rs to back off exponentially on HTTP 429 and added a test. "
                .repeat(12),
        ),
    ]
    .iter()
    .enumerate()
    {
        c.execute(
            "INSERT INTO events(session_id, record_key, kind, text, turn, ts, source_path) VALUES ('pi:s1', ?1, ?2, ?3, 1, 1, '/x.jsonl')",
            rusqlite::params![format!("k{i}"), kind, text],
        )
        .unwrap();
    }
    drop(c);
    let out = mnem(
        &home,
        &["distill", "--since-days", "100000", "--limit", "5"],
    );
    assert!(out.contains("1 observations"), "distill:\n{out}");
    let args = std::fs::read_to_string(home.join("claude-distill.args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let has = |f: &str, v: &str| args.windows(2).any(|w| w[0] == f && w[1] == v);
    assert!(
        has("--model", "sonnet") && has("--tools", "") && has("--setting-sources", ""),
        "{args:?}"
    );
    assert!(args.contains(&"--no-session-persistence") && args.contains(&"--strict-mcp-config"));
    let stdin = std::fs::read_to_string(home.join("claude-distill.stdin")).unwrap();
    assert!(
        stdin.contains("backs off on 429"),
        "the digest goes on stdin"
    );
    let cwd = std::fs::read_to_string(home.join("claude-distill.cwd")).unwrap();
    assert!(
        cwd.contains("mnem-distill-"),
        "runs in its own empty directory: {cwd}"
    );
    let c = mnem::db::open(&home.join(".mnem/mnem.db")).unwrap();
    let title: String = c
        .query_row(
            "SELECT title FROM memories WHERE origin = 'mnem'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(title, "Fake memory from the CLI provider");
    drop(c);

    // A user's own setup is never replaced.
    std::fs::write(
        home.join(".mnem/config.json"),
        r#"{"distill": {"api_key_env": "MY_KEY", "base_url": "http://x/v1"}}"#,
    )
    .unwrap();
    let report = mnem(&home, &["install", "--only", "pi"]);
    assert!(
        !report.contains("distillation: using"),
        "install report:\n{report}"
    );
    let cfg = std::fs::read_to_string(home.join(".mnem/config.json")).unwrap();
    assert!(
        cfg.contains("MY_KEY") && !cfg.contains("claude-cli"),
        "{cfg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The Claude Code plugin's hooks and tools run through `--plugin`: they work when the
/// plugin is the only setup, and stay quiet when `mnem install` already wired the same
/// hook or registered the tools, so nothing runs twice.
#[test]
fn plugin_hooks_and_tools_never_run_twice() {
    let home = scratch("plugin");
    let run = |args: &[&str], stdin: &str| -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mnem"))
            .args(args)
            .env("HOME", &home)
            .env("MNEM_HOME", home.join(".mnem"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        // A plugin hook whose hook is already installed exits without reading its input:
        // it may be gone before the write, which then fails with a broken pipe.
        if let Err(e) = child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
            assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe, "{e}");
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "mnem {args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let tools = |args: &[&str]| -> usize {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n"
        );
        let out = run(args, input);
        let line = out.lines().find(|l| l.contains(r#""id":2"#)).unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        v["result"]["tools"].as_array().unwrap().len()
    };
    let session_start =
        r#"{"session_id":"p1","cwd":"/tmp","hook_event_name":"SessionStart","source":"startup"}"#;
    // The plugin alone: its hook answers and its tools are listed.
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    assert!(
        !run(
            &["hook", "claude", "session-start", "--plugin"],
            session_start
        )
        .is_empty()
    );
    let all = tools(&["mcp"]);
    assert!(all > 0);
    assert_eq!(tools(&["mcp", "--plugin"]), all);
    // After mnem install wired Claude Code: the plugin's copies stay quiet.
    mnem(&home, &["install", "--only", "claude"]);
    assert!(
        run(
            &["hook", "claude", "session-start", "--plugin"],
            session_start
        )
        .is_empty()
    );
    assert_eq!(tools(&["mcp", "--plugin"]), 0);
    // mnem's own hooks and tools are unaffected.
    assert!(!run(&["hook", "claude", "session-start"], session_start).is_empty());
    assert_eq!(tools(&["mcp"]), all);
    let _ = std::fs::remove_dir_all(&home);
}
