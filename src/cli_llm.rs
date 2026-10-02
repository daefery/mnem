//! Distillation through a coding agent's own command line (`claude -p`, `codex exec`),
//! signed in as the user already is: no API key, no proxy.
//!
//! Every run is isolated so it can never become a session mnem captures, and never
//! carries the user's own instructions or tools: an empty working directory, no saved
//! session, no tools, no MCP servers, no hooks (mnem's own included) and no user
//! settings. Chosen on 30 real session chunks judged against luna through an endpoint:
//! sonnet and codex with luna matched it; haiku made up more details and lost.

use crate::distill::Failure;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Which command a provider runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    Claude,
    Codex,
}

impl Cli {
    pub fn from_name(name: &str) -> Option<Cli> {
        match name {
            "claude-cli" => Some(Cli::Claude),
            "codex-cli" => Some(Cli::Codex),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Cli::Claude => "claude-cli",
            Cli::Codex => "codex-cli",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
        }
    }

    /// The model chain when the settings name none (see the module note for why).
    pub fn default_chain(self) -> &'static [&'static str] {
        match self {
            Cli::Claude => &["sonnet"],
            Cli::Codex => &["gpt-5.6-luna"],
        }
    }

    /// The arguments for one request; the prompt itself goes on stdin. `out` is where
    /// codex writes its final message.
    pub fn args(self, model: &str, system: &str, cwd: &Path, out: &Path) -> Vec<String> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match self {
            Cli::Claude => {
                let mut a = s(&["-p", "--model", model, "--effort", "low"]);
                a.extend(s(&["--system-prompt", system]));
                a.extend(s(&[
                    "--tools",
                    "",
                    "--strict-mcp-config",
                    "--setting-sources",
                    "",
                    "--no-session-persistence",
                    "--output-format",
                    "json",
                ]));
                a
            }
            Cli::Codex => {
                let mut a = s(&[
                    "exec",
                    "--ephemeral",
                    "--ignore-user-config",
                    "--disable",
                    "hooks",
                    "--skip-git-repo-check",
                    "--sandbox",
                    "read-only",
                    "-C",
                ]);
                a.push(cwd.to_string_lossy().into_owned());
                a.extend(s(&[
                    "-m",
                    model,
                    "-c",
                    "model_reasoning_effort=\"low\"",
                    "-o",
                ]));
                a.push(out.to_string_lossy().into_owned());
                a.push("-".into());
                a
            }
        }
    }

    /// What goes on stdin: codex has no system-prompt flag, so the instructions lead.
    pub fn stdin(self, system: &str, user: &str) -> String {
        match self {
            Cli::Claude => user.to_string(),
            Cli::Codex => format!("{system}\n\n{user}"),
        }
    }

    /// The reply text from a finished run: claude prints a JSON envelope, codex writes
    /// its last message to `out`. Err says why there is no usable reply.
    pub fn reply(self, stdout: &str, out: &Path) -> std::result::Result<String, Failure> {
        match self {
            Cli::Claude => {
                let v: Value = serde_json::from_str(stdout.trim())
                    .map_err(|_| Failure::NextModel(0, "claude printed no JSON envelope".into()))?;
                let text = v["result"].as_str().unwrap_or_default().to_string();
                if v["is_error"].as_bool() == Some(true) {
                    return Err(classify_message(&text));
                }
                Ok(text)
            }
            Cli::Codex => std::fs::read_to_string(out)
                .ok()
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| Failure::NextModel(0, "codex wrote no final message".into())),
        }
    }
}

/// Map an error message from a command line to what the chain does next: a quota or
/// rate limit rests the model, a missing login stops the run (no other model helps).
pub fn classify_message(msg: &str) -> Failure {
    const MIN: i64 = 60_000;
    let m = msg.to_lowercase();
    let short = crate::text::head(msg.lines().next().unwrap_or(""), 160);
    if [
        "not logged in",
        "please log in",
        "/login",
        "authentication",
        "unauthorized",
        "invalid api key",
    ]
    .iter()
    .any(|k| m.contains(k))
    {
        Failure::Endpoint(format!("not signed in: {short}"))
    } else if [
        "usage limit",
        "rate limit",
        "quota",
        "limit reached",
        "too many requests",
        "429",
    ]
    .iter()
    .any(|k| m.contains(k))
    {
        Failure::NextModel(30 * MIN, format!("quota/rate limit: {short}"))
    } else if [
        "model",
        "not found",
        "not available",
        "does not exist",
        "unsupported",
    ]
    .iter()
    .all(|k| !m.contains(k))
    {
        Failure::NextModel(5 * MIN, short)
    } else {
        Failure::NextModel(6 * 60 * MIN, format!("model unavailable: {short}"))
    }
}

/// The command's path, found the way `mnem install` finds agents (the background
/// service's PATH rarely includes ~/.local/bin or nvm folders).
pub fn locate(cli: Cli) -> Option<PathBuf> {
    crate::agents::command_path(cli.command())
}

/// Run one request through `cli` with `model`; the reply text, or why there is none.
pub fn call(
    cli: Cli,
    model: &str,
    system: &str,
    user: &str,
    timeout: Duration,
) -> std::result::Result<String, Failure> {
    let exe = locate(cli).ok_or_else(|| {
        Failure::Endpoint(format!(
            "`{}` is not installed (or not on any known path)",
            cli.command()
        ))
    })?;
    // An empty directory of its own: no CLAUDE.md or AGENTS.md is read, nothing written
    // lands in a project, and codex's last-message file has a private home.
    let dir = std::env::temp_dir().join(format!(
        "mnem-distill-{}-{}",
        std::process::id(),
        crate::db::now_ms()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| Failure::Endpoint(format!("temp dir: {e}")))?;
    let out = dir.join("reply.txt");
    let result = run(&exe, cli, model, system, user, &dir, &out, timeout);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[allow(clippy::too_many_arguments)]
fn run(
    exe: &Path,
    cli: Cli,
    model: &str,
    system: &str,
    user: &str,
    dir: &Path,
    out: &Path,
    timeout: Duration,
) -> std::result::Result<String, Failure> {
    let mut child = Command::new(exe)
        .args(cli.args(model, system, dir, out))
        .current_dir(dir)
        // Thinking costs several times the answer and did not improve memories.
        .env("MAX_THINKING_TOKENS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Failure::Endpoint(format!("start {}: {e}", exe.display())))?;
    let input = cli.stdin(system, user);
    let mut stdin = child.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    // Read both pipes on their own threads so a chatty command never blocks on a full pipe.
    let read = |p: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = p {
                let _ = p.read_to_string(&mut s);
            }
            s
        })
    };
    let so = read(child.stdout.take().map(|p| Box::new(p) as _));
    let se = read(child.stderr.take().map(|p| Box::new(p) as _));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::NextModel(5 * 60_000, "timeout".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(Failure::Endpoint(format!("wait: {e}"))),
        }
    };
    let _ = writer.join();
    let stdout = so.join().unwrap_or_default();
    let stderr = se.join().unwrap_or_default();
    match cli.reply(&stdout, out) {
        Ok(text) => Ok(text),
        // A failed run explains itself on stderr (or, for claude, in its envelope).
        Err(_) if !status.success() => Err(classify_message(if stderr.trim().is_empty() {
            &stdout
        } else {
            &stderr
        })),
        Err(f) => Err(f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(a: &[String], flag: &str, value: &str) -> bool {
        a.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn every_run_is_isolated_from_the_users_setup_and_from_mnem() {
        let (dir, out) = (Path::new("/tmp/d"), Path::new("/tmp/d/reply.txt"));
        let c = Cli::Claude.args("sonnet", "SYS", dir, out);
        assert!(has(&c, "--tools", "") && has(&c, "--setting-sources", ""));
        assert!(c.contains(&"--strict-mcp-config".into()));
        assert!(c.contains(&"--no-session-persistence".into()));
        assert!(has(&c, "--system-prompt", "SYS") && has(&c, "--model", "sonnet"));
        let x = Cli::Codex.args("gpt-5.6-luna", "SYS", dir, out);
        // Hooks off: otherwise codex runs mnem's own hooks and every request would
        // become a session mnem captures and distils.
        assert!(has(&x, "--disable", "hooks"));
        assert!(x.contains(&"--ephemeral".into()) && x.contains(&"--ignore-user-config".into()));
        assert!(has(&x, "--sandbox", "read-only") && has(&x, "-C", "/tmp/d"));
        assert!(has(&x, "-o", "/tmp/d/reply.txt") && x.last().map(String::as_str) == Some("-"));
        assert_eq!(Cli::Codex.stdin("SYS", "U"), "SYS\n\nU");
        assert_eq!(Cli::Claude.stdin("SYS", "U"), "U");
    }

    #[test]
    fn replies_and_failures_are_read_from_each_command() {
        let none = Path::new("/nonexistent/reply.txt");
        let ok =
            r#"{"type":"result","is_error":false,"result":"```json\n{\"observations\": []}\n```"}"#;
        assert!(
            Cli::Claude
                .reply(ok, none)
                .unwrap()
                .contains("observations")
        );
        let limit = r#"{"type":"result","is_error":true,"result":"Claude AI usage limit reached"}"#;
        assert!(
            matches!(Cli::Claude.reply(limit, none), Err(Failure::NextModel(m, _)) if m >= 30 * 60_000)
        );
        assert!(matches!(
            Cli::Claude.reply("garbage", none),
            Err(Failure::NextModel(0, _))
        ));
        assert!(matches!(
            Cli::Codex.reply("", none),
            Err(Failure::NextModel(0, _))
        ));
        assert!(matches!(
            classify_message("Not logged in · Please run /login"),
            Failure::Endpoint(_)
        ));
        assert!(
            matches!(classify_message("model 'x' not found"), Failure::NextModel(m, _) if m == 6 * 60 * 60_000)
        );
        assert!(
            matches!(classify_message("connection reset"), Failure::NextModel(m, _) if m == 5 * 60_000)
        );
    }

    #[test]
    fn names_round_trip() {
        for c in [Cli::Claude, Cli::Codex] {
            assert_eq!(Cli::from_name(c.name()), Some(c));
        }
        assert_eq!(Cli::from_name("openai"), None);
    }
}
