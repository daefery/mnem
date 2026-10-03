//! Which coding agents are connected to mnem on this machine, and what is missing.
//!
//! One check shared by `mnem install` (which prints what it could not do), `mnem doctor`
//! and the viewer (which offers to connect an agent installed after mnem). Each agent is
//! looked at the same three ways: is it installed, are mnem's hooks (or pi's extension)
//! in its settings, and are mnem's tools registered. Codex adds a fourth: it runs a hook
//! only once the user has trusted that exact definition, which mnem cannot do for them.
//! Everything here reads files; nothing runs an agent.

use crate::db;
use crate::ingest;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Pi,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Pi];

    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Pi => "pi",
        }
    }

    pub fn parse(s: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.id() == s)
    }
}

/// Where one agent stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub agent: Agent,
    pub name: &'static str,
    /// The agent is on this machine (its settings folder or its command exists).
    pub installed: bool,
    /// Memory is given to it automatically: mnem's hooks, or pi's extension.
    pub hooks: bool,
    /// It can call mnem's tools (search, get_observations, recall_file...).
    pub tools: bool,
    /// Codex only: every mnem hook is trusted (None when it cannot be told or n/a).
    pub trusted: Option<bool>,
    /// Hooks point at a mnem binary that no longer exists.
    pub stale_binary: bool,
    /// connected | not_installed | needs_install | needs_trust
    pub state: &'static str,
    /// What to do, in words, when not connected.
    pub action: Option<String>,
}

/// Hook commands mnem writes contain this.
const HOOK_MARK: &str = "mnem hook ";

pub fn status_all() -> Vec<Status> {
    Agent::ALL.into_iter().map(status).collect()
}

pub fn status(agent: Agent) -> Status {
    let (installed, hooks, tools, trusted, bins) = match agent {
        Agent::Claude => claude(),
        Agent::Codex => codex(),
        Agent::Pi => pi(),
    };
    let stale_binary = bins
        .iter()
        .any(|b| Path::new(b).is_absolute() && !is_executable(Path::new(b)));
    let (state, action) = if !installed {
        // Nothing to connect yet; if mnem already wrote its part, it works from the start.
        ("not_installed", None)
    } else if !hooks || !tools || stale_binary {
        (
            "needs_install",
            Some(format!(
                "Connect {}: press Connect in the viewer, or run `mnem install --only {}`",
                agent.name(),
                agent.id()
            )),
        )
    } else if trusted == Some(false) {
        (
            "needs_trust",
            Some(
                "Open Codex and trust mnem's hooks: type /hooks, review them and trust them (Codex skips hooks it has not been told to trust)"
                    .into(),
            ),
        )
    } else {
        ("connected", None)
    };
    Status {
        agent,
        name: agent.name(),
        installed,
        hooks,
        tools,
        trusted,
        stale_binary,
        state,
        action,
    }
}

type Found = (bool, bool, bool, Option<bool>, Vec<String>);

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The mnem hook commands in a Claude-style `{"hooks": {Event: [group]}}` document.
fn hook_commands(doc: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(events) = doc.get("hooks").and_then(Value::as_object) {
        for groups in events.values().filter_map(Value::as_array) {
            for g in groups {
                for h in g
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(c) = h.get("command").and_then(Value::as_str)
                        && c.contains(HOOK_MARK)
                    {
                        out.push(c.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The binary a hook command runs (everything before " hook ").
fn binary_of(command: &str) -> Option<String> {
    command
        .split_once(" hook ")
        .map(|(b, _)| b.trim().trim_matches('"').to_string())
        .filter(|b| b.starts_with('/'))
}

/// A command on PATH, or in the places installers put them when PATH (a service's)
/// does not include them.
pub fn command_path(cmd: &str) -> Option<PathBuf> {
    let home = db::home();
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend([
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".npm-global/bin"),
        home.join(".bun/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ]);
    // nvm keeps one bin folder per Node version.
    if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut v: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.path().join("bin"))
            .collect();
        v.sort();
        v.reverse();
        dirs.extend(v);
    }
    dirs.into_iter()
        // Only absolute folders: a relative PATH entry depends on where mnem runs.
        .filter(|d| d.is_absolute())
        .map(|d| d.join(cmd))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.is_file() && m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        m.is_file()
    }
}

/// Whether every hook mnem installs for `agent` is in `doc`, with this event and command
/// (whatever mnem binary it names).
fn all_hooks(doc: &Value, agent: &str) -> bool {
    crate::install::hook_entries("", agent)
        .iter()
        .all(|(event, _, cmd, _)| {
            // `cmd` is " hook <agent> <event>" with an empty binary.
            doc.get("hooks")
                .and_then(|h| h.get(*event))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|g| {
                    g.get("hooks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter_map(|h| h.get("command").and_then(Value::as_str))
                .any(|c| c.ends_with(cmd.as_str()))
        })
}

/// The Claude Code profile a hook or MCP server started by Claude Code runs under:
/// CLAUDE_CONFIG_DIR when set, else ~/.claude.
pub fn active_claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| db::home().join(".claude"))
}

/// Whether `mnem install` already put mnem's hook for `event` (session-start, prompt,
/// stop, file) into the active profile's settings: the plugin's copy then stays quiet,
/// so a hook never runs twice.
pub fn settings_has_hook(event: &str) -> bool {
    let doc = read_json(&active_claude_dir().join("settings.json")).unwrap_or(Value::Null);
    let suffix = format!(" hook claude {event}");
    hook_commands(&doc).iter().any(|c| c.ends_with(&suffix))
}

/// Whether the active profile already has mnem's tools registered by `mnem install`.
pub fn settings_has_tools() -> bool {
    crate::install::claude_mcp_command(&active_claude_dir()).is_some()
}

fn claude() -> Found {
    let dirs: Vec<PathBuf> = ingest::claude_config_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .collect();
    // mnem may create a settings folder and a bare state file; only Claude Code creates
    // projects/ or puts anything else in its state file.
    let installed = command_path("claude").is_some()
        || dirs.iter().any(|d| {
            d.join("projects").is_dir()
                || read_json(&crate::install::claude_state(d))
                    .is_some_and(|v| !crate::install::only_mnem_state(&v))
        });
    // Every profile must be wired: a profile without hooks gets no memory.
    let mut hooks = !dirs.is_empty();
    let mut tools = !dirs.is_empty();
    let mut bins = Vec::new();
    for d in &dirs {
        let doc = read_json(&d.join("settings.json")).unwrap_or(Value::Null);
        hooks &= all_hooks(&doc, "claude");
        bins.extend(hook_commands(&doc).iter().filter_map(|c| binary_of(c)));
        match crate::install::claude_mcp_command(d) {
            Some(c) => bins.push(c),
            None => tools = false,
        }
    }
    (installed, hooks, tools, None, bins)
}

fn codex() -> Found {
    let dir = db::home().join(".codex");
    // mnem writes hooks.json and config.toml itself; only Codex writes sessions/.
    let installed = command_path("codex").is_some() || dir.join("sessions").is_dir();
    let hooks_path = dir.join("hooks.json");
    let doc = read_json(&hooks_path).unwrap_or(Value::Null);
    let cmds = hook_commands(&doc);
    let hooks = all_hooks(&doc, "codex");
    // Unreadable or invalid settings read as nothing wired (and are reported so).
    let cfg: toml_edit::DocumentMut = std::fs::read_to_string(dir.join("config.toml"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    let server = codex_mcp_command(&cfg);
    let trusted = hooks.then(|| codex_trusted(&cfg, &hooks_path));
    let mut bins: Vec<String> = cmds.iter().filter_map(|c| binary_of(c)).collect();
    bins.extend(server.clone());
    (installed, hooks, server.is_some(), trusted, bins)
}

/// The command of `[mcp_servers.mnem]` when it runs mnem's MCP server (`args` = `["mcp"]`).
pub(crate) fn codex_mcp_command(cfg: &toml_edit::DocumentMut) -> Option<String> {
    let m = cfg.get("mcp_servers")?.get("mnem")?;
    let args = m.get("args")?.as_array()?;
    (args.len() == 1 && args.get(0)?.as_str() == Some("mcp"))
        .then(|| m.get("command")?.as_str().map(str::to_string))
        .flatten()
}

/// Codex records trust per hook as `[hooks.state."<hooks.json>:<event>:<group>:<hook>"]`
/// with a `trusted_hash` (of the exact definition, so a changed hook needs trusting
/// again). mnem cannot recompute that hash; it checks that each of its hooks has a
/// trust entry at its position. A definition changed since trusting still reads as
/// trusted here, and Codex itself asks again on its next start.
fn codex_trusted(cfg: &toml_edit::DocumentMut, hooks_path: &Path) -> bool {
    let Some(doc) = read_json(hooks_path) else {
        return false;
    };
    let Some(events) = doc.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    let state = cfg.get("hooks").and_then(|h| h.get("state"));
    let file = hooks_path.to_string_lossy();
    let mut all = true;
    for (event, groups) in events {
        let snake = snake_case(event);
        for (gi, g) in groups.as_array().into_iter().flatten().enumerate() {
            for (hi, h) in g
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let ours = h
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(HOOK_MARK));
                if !ours {
                    continue;
                }
                let key = format!("{file}:{snake}:{gi}:{hi}");
                all &= state
                    .and_then(|s| s.get(&key))
                    .and_then(|t| t.get("trusted_hash"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| !v.is_empty());
            }
        }
    }
    all
}

/// SessionStart -> session_start, UserPromptSubmit -> user_prompt_submit.
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn pi() -> Found {
    let home = db::home();
    // mnem writes the extension itself; only pi writes sessions/.
    let installed = command_path("pi").is_some() || home.join(".pi/agent/sessions").is_dir();
    let ext = home.join(".pi/agent/extensions/mnem/index.ts");
    let src = std::fs::read_to_string(&ext).unwrap_or_default();
    let wired = src.contains("MNEM_BIN");
    // The extension names the binary as a JSON string literal after `?? `.
    let bins = src
        .lines()
        .find(|l| l.contains("process.env.MNEM_BIN ??"))
        .and_then(|l| l.split("?? ").nth(1))
        .and_then(|s| serde_json::from_str::<String>(s.trim_end_matches(';')).ok())
        .into_iter()
        .collect();
    // The extension carries both: memory at session start and on files, and the tools.
    (installed, wired, wired, None, bins)
}

/// One line per agent, for `mnem doctor` and the end of `mnem install`.
pub fn render(statuses: &[Status]) -> String {
    let mut w = String::new();
    for s in statuses {
        let what = match s.state {
            "connected" => "connected".to_string(),
            "not_installed" if s.hooks && s.tools => {
                "not installed yet; mnem is ready for it (nothing to do when you install it)".into()
            }
            "not_installed" => {
                "not installed (run `mnem install` again after installing it)".into()
            }
            "needs_trust" => {
                "hooks written, not trusted yet: in Codex type /hooks and trust them".into()
            }
            _ => {
                let mut missing = Vec::new();
                if !s.hooks {
                    missing.push("memory hooks");
                }
                if !s.tools {
                    missing.push("tools");
                }
                if s.stale_binary {
                    missing.push("points at a mnem binary that no longer exists");
                }
                format!(
                    "NOT CONNECTED ({}): run `mnem install --only {}`",
                    missing.join(", "),
                    s.agent.id()
                )
            }
        };
        w.push_str(&format!("  {:<12} {what}\n", s.name));
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_mnem_hooks_and_their_binary() {
        let doc = json!({ "hooks": {
            "Stop": [{ "hooks": [
                { "type": "command", "command": "notify-send done" },
                { "type": "command", "command": "/opt/mnem hook claude stop" }
            ] }],
        } });
        let cmds = hook_commands(&doc);
        assert_eq!(cmds, ["/opt/mnem hook claude stop"]);
        assert_eq!(binary_of(&cmds[0]).as_deref(), Some("/opt/mnem"));
        assert!(hook_commands(&json!({})).is_empty());
    }

    #[test]
    fn codex_trust_needs_an_entry_for_every_mnem_hook() {
        let dir = std::env::temp_dir().join(format!("mnem-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hooks = dir.join("hooks.json");
        std::fs::write(
            &hooks,
            json!({ "hooks": {
                "SessionStart": [
                    { "hooks": [{ "type": "command", "command": "gh-axi" }] },
                    { "hooks": [{ "type": "command", "command": "/x/mnem hook codex session-start" }] }
                ],
                "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "/x/mnem hook codex prompt" }] }],
            } })
            .to_string(),
        )
        .unwrap();
        let f = hooks.to_string_lossy();
        let doc = |s: String| -> toml_edit::DocumentMut { s.parse().unwrap() };
        let one = format!("[hooks.state.\"{f}:session_start:1:0\"]\ntrusted_hash = \"a\"\n");
        assert!(
            !codex_trusted(&doc(one.clone()), &hooks),
            "the prompt hook is not trusted"
        );
        let both =
            format!("{one}[hooks.state.\"{f}:user_prompt_submit:0:0\"]\ntrusted_hash = \"b\"\n");
        assert!(codex_trusted(&doc(both), &hooks));
        // Another hook's trust (gh-axi at 0:0) does not count for mnem's.
        let wrong = format!(
            "[hooks.state.\"{f}:session_start:0:0\"]\ntrusted_hash = \"a\"\n[hooks.state.\"{f}:user_prompt_submit:0:0\"]\ntrusted_hash = \"b\"\n"
        );
        assert!(!codex_trusted(&doc(wrong), &hooks));
        // A commented-out entry is not trust; the inline-table form is.
        let commented = format!(
            "# [hooks.state.\"{f}:session_start:1:0\"]\n# trusted_hash = \"a\"\n[hooks.state.\"{f}:user_prompt_submit:0:0\"]\ntrusted_hash = \"b\"\n"
        );
        assert!(!codex_trusted(&doc(commented), &hooks));
        let inline = format!(
            "[hooks.state]\n\"{f}:session_start:1:0\" = {{ trusted_hash = \"a\" }}\n\"{f}:user_prompt_submit:0:0\" = {{ trusted_hash = \"b\" }}\n"
        );
        assert!(codex_trusted(&doc(inline), &hooks));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn states_follow_what_is_found() {
        assert_eq!(snake_case("UserPromptSubmit"), "user_prompt_submit");
        let cfg = |s: &str| -> toml_edit::DocumentMut { s.parse().unwrap() };
        assert_eq!(
            codex_mcp_command(&cfg(
                "[mcp_servers.mnem]\ncommand = \"/m\"\nargs = [\n  \"mcp\",\n]\n"
            ))
            .as_deref(),
            Some("/m")
        );
        assert_eq!(
            codex_mcp_command(&cfg("[mcp_servers.mnem]\n")),
            None,
            "empty table"
        );
        assert_eq!(
            codex_mcp_command(&cfg("# [mcp_servers.mnem]\n# command = \"/m\"\n")),
            None
        );
        assert_eq!(
            codex_mcp_command(&cfg(
                "[mcp_servers.mnem]\ncommand = \"/m\"\nargs = [\"mcp\", \"x\"]\n"
            )),
            None
        );
    }
}
