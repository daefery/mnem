//! `rvn install`: wire ravnori into Claude Code, Codex and pi.
//!
//! Every file is backed up before it is changed (`<file>.bak-ravnori-<ts>`), entries are
//! idempotent (re-running replaces ravnori's own entries and nothing else), and
//! `--dry-run` prints the plan without writing.

use crate::db;
use crate::ingest;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

// What install reports: printed for the command line, collected for the viewer.
thread_local! {
    static LOG: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

fn say_raw(line: &str) {
    let line = line.trim_end_matches('\n');
    let collected = LOG.with(|l| {
        if let Some(v) = l.borrow_mut().as_mut() {
            v.extend(line.lines().map(str::to_string));
            true
        } else {
            false
        }
    });
    if !collected {
        println!("{line}");
    }
}

macro_rules! say {
    ($($t:tt)*) => { say_raw(&format!($($t)*)) };
}

/// Run `f` with install's report collected instead of printed. The previous collector
/// (if any) is restored afterwards, also when `f` panics.
pub fn collecting<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    struct Restore(Option<Vec<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            LOG.with(|l| *l.borrow_mut() = prev);
        }
    }
    let _restore = Restore(LOG.with(|l| l.borrow_mut().replace(Vec::new())));
    let out = f();
    let lines = LOG.with(|l| l.borrow_mut().take().unwrap_or_default());
    (out, lines)
}

pub struct Plan {
    pub bin: String,
    pub dry_run: bool,
    pub claude: bool,
    pub codex: bool,
    pub pi: bool,
    pub watch: bool,
}

pub fn run(p: &Plan) -> Result<()> {
    from_mnem(p)?;
    if p.claude {
        claude(p)?;
    }
    if p.codex {
        codex(p)?;
    }
    if p.pi {
        pi(p)?;
    }
    if p.watch {
        watch_service(p)?;
    }
    distillation(p)?;
    if p.dry_run {
        say!("\n(dry run: nothing written)");
    } else {
        say!("\nagents");
        say_raw(&crate::agents::render(&crate::agents::status_all()));
    }
    Ok(())
}

/// An install from before 0.6.0, when ravnori was called mnem: move its data and take
/// down what only mnem names (MCP server, pi extension, service). The steps after this
/// write ravnori's own, replacing mnem's hooks in place.
fn from_mnem(p: &Plan) -> Result<()> {
    use crate::rename;
    let states: Vec<(PathBuf, PathBuf)> = claude_dirs()
        .into_iter()
        .map(|d| {
            let s = claude_state(&d);
            (d, s)
        })
        .filter(|(_, s)| rename::has_old_claude_mcp(s))
        .collect();
    if !rename::needed() && states.is_empty() && !rename::has_old_codex_mcp() {
        // Still link a stray old command (a `cargo install`ed mnem with nothing else).
        for n in rename::link_old_command(&p.bin, p.dry_run)? {
            say!("{n}");
        }
        return Ok(());
    }
    say!("moving from mnem (ravnori's name before 0.6.0)");
    let mut notes = vec![
        rename::move_data(p.dry_run)?,
        rename::remove_old_service(p.dry_run)?,
        rename::remove_old_pi_extension(p.dry_run)?,
        rename::remove_old_codex_mcp(p.dry_run)?,
    ];
    for (dir, state) in states {
        // Through Claude Code's own command when it is there; the state file is checked
        // afterwards and cleaned directly if the entry is still in it.
        if !p.dry_run
            && let Some(exe) = crate::agents::command_path("claude")
        {
            let _ = Command::new(exe)
                .args(["mcp", "remove", "--scope", "user", rename::OLD])
                .envs(claude_env(&dir))
                .output();
        }
        notes.push(if p.dry_run || rename::has_old_claude_mcp(&state) {
            rename::remove_old_claude_mcp_file(&state, p.dry_run)?
        } else {
            Some(format!(
                "removed the mnem MCP server from Claude Code ({})",
                dir.display()
            ))
        });
    }
    notes.extend(
        rename::link_old_command(&p.bin, p.dry_run)?
            .into_iter()
            .map(Some),
    );
    for n in notes.into_iter().flatten() {
        say!("  {n}");
    }
    Ok(())
}

/// Distillation needs a model. With none configured, use a coding agent's own command
/// line, signed in as the user already is: Claude Code first (most teams have it), else
/// Codex. Only `distill.provider` is written (and the daily cap, for a new user), other
/// settings are kept; an existing configuration is never changed.
fn distillation(p: &Plan) -> Result<()> {
    let cfg = &crate::config::CONFIG.distill;
    if crate::distill::not_configured(cfg).is_none() {
        return Ok(());
    }
    let found: Vec<crate::cli_llm::Cli> = [crate::cli_llm::Cli::Claude, crate::cli_llm::Cli::Codex]
        .into_iter()
        .filter(|c| crate::cli_llm::locate(*c).is_some())
        .collect();
    if found.is_empty() {
        if let Some(why) = crate::distill::not_configured(cfg) {
            say!("\n{why}");
        }
        return Ok(());
    }
    // A command line being installed does not mean it can answer: its login may be
    // missing or expired. One short request says so now, not at the first answer.
    let mut failures = Vec::new();
    let works = if p.dry_run {
        None
    } else {
        found.iter().copied().find(|c| match answers(*c) {
            Ok(()) => true,
            Err(why) => {
                failures.push(why);
                false
            }
        })
    };
    let cli = works.unwrap_or(found[0]);
    say!(
        "\ndistillation: using `{}` ({}); at most {} background requests a day",
        cli.command(),
        cli.default_chain().join(", "),
        NEW_USER_DAILY_CALLS
    );
    if p.dry_run {
        say!("  not tested (dry run)");
        return Ok(());
    }
    match works {
        Some(_) => say!("  tested: it answered"),
        None => {
            say!("  ! it did not answer, so memories and `rvn ask` answers wait until it does:");
            for f in &failures {
                say!("    {f}");
            }
            say!("  then run `rvn doctor` to check");
        }
    }
    let path = crate::config::path();
    let mut v: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("{} is not valid JSON; not changed", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    let d = v
        .as_object_mut()
        .context("settings file is not a JSON object; not changed")?
        .entry("distill")
        .or_insert_with(|| serde_json::json!({}));
    let d = d
        .as_object_mut()
        .context("distill settings are not an object; not changed")?;
    d.insert("provider".into(), cli.name().into());
    d.entry("daily_calls")
        .or_insert_with(|| NEW_USER_DAILY_CALLS.into());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v)? + "\n")?;
    say!("  written to {}", path.display());
    Ok(())
}

/// Whether `cli` answers one short request with its default model: why not, as the
/// user can act on it ("claude is not signed in: open Claude Code and type /login").
fn answers(cli: crate::cli_llm::Cli) -> std::result::Result<(), String> {
    let model = cli.default_chain()[0];
    match crate::cli_llm::call(
        cli,
        model,
        "Reply with JSON only.",
        r#"Reply with exactly: {"ok": true}"#,
        std::time::Duration::from_secs(120),
    ) {
        Ok(_) => Ok(()),
        Err(
            crate::distill::Failure::Endpoint(why) | crate::distill::Failure::NextModel(_, why),
        ) => Err(format!("{}: {why}", cli.command())),
    }
}

/// Background requests a day for a user whose distillation runs on their own plan.
const NEW_USER_DAILY_CALLS: usize = 100;

pub(crate) type HookEntry = (&'static str, Option<&'static str>, String, u64);

pub(crate) fn hook_entries(bin: &str, agent: &str) -> Vec<HookEntry> {
    vec![
        (
            "SessionStart",
            Some("startup|resume|clear|compact"),
            format!("{bin} hook {agent} session-start"),
            15,
        ),
        (
            "UserPromptSubmit",
            None,
            format!("{bin} hook {agent} prompt"),
            10,
        ),
        ("Stop", None, format!("{bin} hook {agent} stop"), 10),
    ]
    .into_iter()
    // Memories about a file when Claude Code first reads or changes it in a session.
    // (Codex edits through apply_patch and asks again to trust changed hooks; it uses
    // the recall_file MCP tool instead.)
    .chain((agent == "claude").then(|| {
        (
            "PostToolUse",
            Some("Read|Edit|Write|MultiEdit|NotebookEdit"),
            format!("{bin} hook {agent} file"),
            10,
        )
    }))
    .collect()
}

fn is_ours(h: &Value) -> bool {
    h.get("command")
        .and_then(Value::as_str)
        .is_some_and(crate::agents::is_hook_command)
}

/// Remove ravnori's hooks everywhere. Other hooks stay, even when they share a group with
/// one of ours; only groups and events left empty by the removal are dropped.
fn strip_ours(doc: &mut Value) -> usize {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    for groups in hooks.values_mut() {
        let Some(arr) = groups.as_array_mut() else {
            continue;
        };
        arr.retain_mut(|g| {
            let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = hs.len();
            hs.retain(|h| !is_ours(h));
            removed += before - hs.len();
            !(before > 0 && hs.is_empty())
        });
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    removed
}

/// Replace ravnori's entries in a Claude-style `{"hooks": {Event: [group]}}` document.
fn merge_hooks(doc: &mut Value, entries: &[HookEntry], extra: &Value) -> Result<()> {
    anyhow::ensure!(doc.is_object(), "settings root is not a JSON object");
    // Already exactly right: leave every hook where it is. Codex keys a hook's trust
    // by its position, so moving an unchanged hook would ask the user to trust it again.
    if ours_match(doc, entries, extra) {
        return Ok(());
    }
    strip_ours(doc);
    let hooks = doc
        .as_object_mut()
        .expect("checked above")
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("\"hooks\" is not a JSON object")?;
    for (event, matcher, command, timeout) in entries {
        let arr = hooks
            .entry(event.to_string())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;
        let mut hook = json!({ "type": "command", "command": command, "timeout": timeout });
        if let (Some(h), Some(x)) = (hook.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                h.insert(k.clone(), v.clone());
            }
        }
        let mut group = json!({ "hooks": [hook] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        arr.push(group);
    }
    Ok(())
}

/// Whether ravnori's hooks in `doc` are exactly `entries`: each entry found once (its own
/// group, same event, matcher, command, timeout and extra keys) and no other rvn hook.
fn ours_match(doc: &Value, entries: &[HookEntry], extra: &Value) -> bool {
    let Some(hooks) = doc.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    let want = |command: &str, timeout: u64| {
        let mut w = json!({ "type": "command", "command": command, "timeout": timeout });
        if let (Some(o), Some(x)) = (w.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                o.insert(k.clone(), v.clone());
            }
        }
        w
    };
    let mut seen = vec![0usize; entries.len()];
    for (event, groups) in hooks {
        for g in groups.as_array().into_iter().flatten() {
            for h in g
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if !is_ours(h) {
                    continue;
                }
                let alone = g
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.len() == 1);
                let Some(i) = entries.iter().position(|(e, matcher, command, timeout)| {
                    e == event
                        && g.get("matcher").and_then(Value::as_str) == *matcher
                        && alone
                        && *h == want(command, *timeout)
                }) else {
                    return false;
                };
                seen[i] += 1;
            }
        }
    }
    seen.iter().all(|n| *n == 1)
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))
        }
        _ => Ok(json!({})),
    }
}

/// Copy `path` beside itself before changing it. Names never collide: two changes in the
/// same millisecond (two viewer requests) get separate copies.
/// Back up a file before ravnori changes it (also used when moving from mnem).
pub(crate) fn backup_file(path: &Path) -> Result<()> {
    backup(path)
}

fn backup(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let ms = db::now_ms();
    for n in 0..100 {
        let name = if n == 0 {
            format!("{}.bak-ravnori-{ms}", path.display())
        } else {
            format!("{}.bak-ravnori-{ms}-{n}", path.display())
        };
        let b = PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&b)
        {
            Ok(mut f) => {
                std::io::copy(&mut std::fs::File::open(path)?, &mut f)?;
                say!("  backup: {}", b.display());
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no free backup name beside {}", path.display())
}

fn write_json(p: &Plan, path: &Path, doc: &Value) -> Result<()> {
    let s = serde_json::to_string_pretty(doc)? + "\n";
    // Unchanged: no backup, no write (and the agent sees no modified file).
    if read_json(path).ok().as_ref() == Some(doc) && path.exists() {
        say!("  unchanged: {}", path.display());
        return Ok(());
    }
    if p.dry_run {
        say!("  would write {} ({} bytes)", path.display(), s.len());
        return Ok(());
    }
    backup(path)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, s)?;
    say!("  wrote {}", path.display());
    Ok(())
}

/// Claude Code profiles to wire: every existing one, and the default one even before
/// Claude Code is installed (it reads what is already there on its first start).
fn claude_dirs() -> Vec<PathBuf> {
    let default = db::home().join(".claude");
    let mut dirs: Vec<PathBuf> = ingest::claude_config_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .collect();
    if !dirs.contains(&default) {
        dirs.insert(0, default);
    }
    dirs
}

fn claude(p: &Plan) -> Result<()> {
    for dir in claude_dirs() {
        say!("Claude Code ({})", dir.display());
        let path = dir.join("settings.json");
        let mut doc = read_json(&path)?;
        merge_hooks(&mut doc, &hook_entries(&p.bin, "claude"), &json!({}))?;
        for (event, _, cmd, _) in hook_entries(&p.bin, "claude") {
            say!("  hook {event}: {cmd}");
        }
        write_json(p, &path, &doc)?;
        let registered = mcp_via_cli(
            p,
            "claude",
            &claude_env(&dir),
            &["mcp", "remove", "--scope", "user", "ravnori"],
            &[
                "mcp", "add", "--scope", "user", "ravnori", "--", &p.bin, "mcp",
            ],
        )?;
        // Checked, not assumed: the command can succeed yet write somewhere else.
        if !registered || (!p.dry_run && !claude_has_mcp(&dir)) {
            claude_mcp_direct(p, &dir)?;
        }
    }
    Ok(())
}

/// A Claude Code state file holding nothing but what ravnori writes before Claude Code is
/// installed (`{"mcpServers": {"ravnori": ...}}`).
pub(crate) fn only_ravnori_state(doc: &Value) -> bool {
    doc.as_object().is_some_and(|o| o.len() == 1)
        && doc
            .get("mcpServers")
            .and_then(Value::as_object)
            .is_some_and(|m| m.len() == 1 && m.contains_key("ravnori"))
}

/// Where a Claude Code profile keeps its user-scope MCP servers: `.claude.json` in the
/// profile folder, or in the home folder for the default profile.
pub(crate) fn claude_state(dir: &Path) -> PathBuf {
    if dir == db::home().join(".claude") {
        db::home().join(".claude.json")
    } else {
        dir.join(".claude.json")
    }
}

/// The command ravnori's tools are registered with in this profile (user scope), if any.
pub(crate) fn claude_mcp_command(dir: &Path) -> Option<String> {
    let v = read_json(&claude_state(dir)).ok()?;
    let m = v.get("mcpServers")?.get("ravnori")?;
    let args_ok = m
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|a| a.first().and_then(Value::as_str) == Some("mcp"));
    args_ok
        .then(|| m.get("command")?.as_str().map(str::to_string))
        .flatten()
}

/// Whether the profile has ravnori's tools registered at user scope.
pub(crate) fn claude_has_mcp(dir: &Path) -> bool {
    claude_mcp_command(dir).is_some()
}

/// Register ravnori's tools before Claude Code exists: create the profile's `.claude.json`
/// with just ravnori's server, as `claude mcp add --scope user` would write it. Only when
/// the file is absent (created exclusively, so a Claude Code that starts meanwhile wins):
/// once Claude Code keeps its state there, only its own `claude mcp` edits it.
fn claude_mcp_direct(p: &Plan, dir: &Path) -> Result<()> {
    let path = claude_state(dir);
    // Already registered (ravnori created this file on an earlier run): nothing to do.
    if claude_mcp_command(dir).as_deref() == Some(p.bin.as_str()) {
        say!("  mcp: already registered in {}", path.display());
        return Ok(());
    }
    if path.exists() {
        say!(
            "  mcp: NOT registered: {} belongs to Claude Code and its `claude` command is not found; run: {}claude mcp add --scope user ravnori -- {} mcp",
            path.display(),
            claude_env(dir)
                .iter()
                .map(|(k, v)| format!("{k}={v} "))
                .collect::<String>(),
            p.bin
        );
        return Ok(());
    }
    let body = serde_json::to_string_pretty(&json!({ "mcpServers": { "ravnori": {
        "type": "stdio", "command": p.bin, "args": ["mcp"], "env": {}
    } } }))?
        + "\n";
    if p.dry_run {
        say!("  would create {} with ravnori's tools", path.display());
        return Ok(());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())?;
            say!(
                "  mcp: created {} with ravnori's tools (Claude Code is not installed yet)",
                path.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            say!(
                "  mcp: {} appeared meanwhile; left to Claude Code",
                path.display()
            );
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// `CLAUDE_CONFIG_DIR` for non-default profiles, so `claude mcp` edits the right one.
fn claude_env(dir: &Path) -> Vec<(String, String)> {
    if dir == db::home().join(".claude") {
        vec![]
    } else {
        vec![(
            "CLAUDE_CONFIG_DIR".into(),
            dir.to_string_lossy().into_owned(),
        )]
    }
}

fn codex(p: &Plan) -> Result<()> {
    say!("Codex");
    let path = db::home().join(".codex/hooks.json");
    let mut doc = read_json(&path)?;
    // Codex caps injected context per hook; ravnori's session context is ~8 KB.
    merge_hooks(
        &mut doc,
        &hook_entries(&p.bin, "codex"),
        &json!({ "additionalContextLimit": 12000 }),
    )?;
    for (event, _, cmd, _) in hook_entries(&p.bin, "codex") {
        say!("  hook {event}: {cmd}");
    }
    write_json(p, &path, &doc)?;
    // MCP server: `[mcp_servers.ravnori]` running this binary. Added when missing; its
    // `command` and `args` corrected in place when they differ (a moved binary), with any
    // other keys the user put in the table kept.
    let cfg = db::home().join(".codex/config.toml");
    let cur = std::fs::read_to_string(&cfg).unwrap_or_default();
    let next = set_ravnori_server(&cur, &p.bin)
        .with_context(|| format!("{} is not valid TOML; left unchanged", cfg.display()))?;
    if next == cur {
        say!("  mcp: [mcp_servers.ravnori] already in {}", cfg.display());
    } else if p.dry_run {
        say!("  would set [mcp_servers.ravnori] in {}", cfg.display());
    } else {
        backup(&cfg)?;
        if let Some(d) = cfg.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&cfg, next)?;
        say!("  mcp: set [mcp_servers.ravnori] in {}", cfg.display());
    }
    Ok(())
}

/// Run the agent's own command to register ravnori's tools. True when it did.
fn mcp_via_cli(
    p: &Plan,
    cli: &str,
    env: &[(String, String)],
    remove: &[&str],
    add: &[&str],
) -> Result<bool> {
    let shown = format!(
        "{}{cli} {}",
        env.iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect::<String>(),
        add.join(" ")
    );
    // Found even when not on PATH (the viewer runs under a service with a short PATH).
    let Some(exe) = crate::agents::command_path(cli) else {
        say!("  mcp: {cli} is not installed yet");
        return Ok(false);
    };
    if p.dry_run {
        say!("  would run: {shown}");
        return Ok(true);
    }
    let run = |args: &[&str]| {
        let mut c = Command::new(&exe);
        c.args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output()
    };
    let _ = run(remove);
    Ok(match run(add) {
        Ok(o) if o.status.success() => {
            say!("  mcp: {shown}");
            true
        }
        Ok(o) => {
            say!(
                "  mcp: `{shown}` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            say!("  mcp: {cli} could not run ({e})");
            false
        }
    })
}

fn pi(p: &Plan) -> Result<()> {
    say!("pi");
    let dir = db::home().join(".pi/agent/extensions/ravnori");
    let path = dir.join("index.ts");
    // JSON string encoding is a valid TS string literal (escapes Windows backslashes).
    let src = PI_EXTENSION.replace("__RAVNORI_BIN__", &serde_json::to_string(&p.bin)?);
    if std::fs::read_to_string(&path).is_ok_and(|cur| cur == src) {
        say!("  unchanged: {}", path.display());
        return Ok(());
    }
    if p.dry_run {
        say!("  would write {} ({} bytes)", path.display(), src.len());
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    backup(&path)?;
    std::fs::write(&path, src)?;
    say!("  wrote {}", path.display());
    Ok(())
}

const PI_EXTENSION: &str = r#"/**
 * ravnori for pi: session-start context, cross-agent updates, and memory tools.
 * Generated by `rvn install`; re-run it to update. Capture itself needs nothing
 * here: ravnori reads pi's own session files.
 */
import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { homedir } from "node:os";
import { resolve } from "node:path";

const RAVNORI = process.env.RAVNORI_BIN ?? __RAVNORI_BIN__;

export default function (pi: ExtensionAPI) {
	let session = "";
	// Files already looked up in this session: no process for a second read of one.
	let touched = new Set<string>();
	let startContext = "";
	let delivered = false;

	const run = async (args: string[]) => {
		try {
			const r = await pi.exec(RAVNORI, args);
			return r.code === 0 ? r.stdout.trim() : "";
		} catch {
			return "";
		}
	};

	// Tools the agent calls: failures are reported, not passed off as "no results", and
	// relative paths start from the session's current directory.
	const call = async (args: string[], signal: AbortSignal | undefined, cwd: string) => {
		const r = await pi.exec(RAVNORI, args, { signal, cwd });
		if (r.code !== 0) throw new Error(`ravnori failed (exit ${r.code}): ${(r.stderr || r.stdout).trim().slice(0, 500)}`);
		return r.stdout.trim();
	};

	pi.on("session_start", async (_event, ctx) => {
		session = `pi:${ctx.sessionManager.getSessionId()}`;
		touched.clear();
		const file = ctx.sessionManager.getSessionFile();
		if (file) await run(["ingest", file]);
		startContext = await run(["context", "--cwd", ctx.cwd, "--session", session]);
		delivered = false;
	});

	// Context rides along as a custom message in the transcript: sent once, kept in the
	// conversation, and leaves pi's own system prompt untouched.
	pi.on("before_agent_start", async (event, ctx) => {
		const file = ctx.sessionManager.getSessionFile();
		if (file) await run(["ingest", file]);
		const parts: string[] = [];
		if (!delivered && startContext) {
			delivered = true;
			parts.push(startContext);
		}
		if (session) {
			const args = ["delta", "--session", session, "--cwd", ctx.cwd];
			if (typeof event.prompt === "string" && event.prompt) args.push("--prompt", event.prompt);
			const update = await run(args);
			if (update) parts.push(update);
		}
		const content = parts.join("\n\n");
		if (content) {
			return { message: { customType: "ravnori-context", content, display: false } };
		}
	});

	// First read or change of a file in this session: past memories about it ride along
	// with the tool's result, as Claude Code's file hook does.
	pi.on("tool_result", async (event, ctx) => {
		if (!session || event.isError) return;
		if (event.toolName !== "read" && event.toolName !== "edit" && event.toolName !== "write") return;
		let path = (event.input as any)?.path;
		if (typeof path !== "string" || !path) return;
		if (path === "~" || path.startsWith("~/")) path = homedir() + path.slice(1);
		const abs = resolve(ctx.cwd, path);
		if (touched.has(abs)) return;
		touched.add(abs);
		let text = "";
		try {
			const r = await pi.exec(RAVNORI, ["file", abs, "--touch", "--session", session, "--cwd", ctx.cwd], {
				cwd: ctx.cwd,
				timeout: 2500,
				signal: ctx.signal,
			});
			text = r.code === 0 && !r.killed ? r.stdout.trim() : "";
		} catch {
			return;
		}
		if (text) return { content: [...event.content, { type: "text", text: `\n\n${text}` }] };
	});

	// End of a turn: record the git working tree so the next session knows what is in progress.
	pi.on("agent_end", async (_event, ctx) => {
		// Not awaited: git on a big repo must never delay the next turn.
		if (session) void run(["snapshot", "--session", session, "--cwd", ctx.cwd]);
	});

	const tool = (name: string, description: string, parameters: any, promptSnippet?: string) =>
		defineTool({
			name: `ravnori_${name}`,
			label: `ravnori ${name}`,
			description,
			promptSnippet,
			parameters,
			async execute(_id, params, signal, _onUpdate, ctx) {
				const args: any = { ...(params ?? {}) };
				if (name === "ask" && !args.cwd) args.cwd = ctx.cwd;
				if (name === "recall_file") {
					if (!args.cwd) args.cwd = ctx.cwd;
					if (session) args.session = session;
				}
				const text = await call(["tool", name, JSON.stringify(args)], signal, ctx.cwd);
				return { content: [{ type: "text", text: text || "No results." }], details: {} };
			},
		});

	pi.registerTool(
		tool("search", "Search past work across all agents (decisions, bugs, fixes, what was tried): use when the user refers to earlier work (\"like last time\", \"that bug\", \"why did we\"), or when unsure whether a design question was already settled in this project (pass project). Not for general programming knowledge. Returns a one-line-per-hit index with ids for ravnori_get_observations.", Type.Object({
			query: Type.String({ description: "Search query" }),
			project: Type.Optional(Type.String({ description: "Filter by project (substring)" })),
			type: Type.Optional(Type.String({ description: "observations | sessions | prompts | events" })),
			limit: Type.Optional(Type.Number({ description: "Max results (default 20)" })),
		}), "ravnori_search: search past work (decisions, bugs, fixes) when the user refers to earlier work"),
	);
	pi.registerTool(
		tool("ask", "Answer a question about past work from the record, with sources: why something was decided, what was done on a day or in a week (\"what did we do yesterday\", \"what shipped on 4 October\"), what was tried. Reads time words itself and, for a question about a time, looks in every project unless one is named; says when nothing is recorded. Use it before reconstructing history from git log, notes or tickets.", Type.Object({
			question: Type.String({ description: "The question in plain words, as the user asked it (English or Indonesian)" }),
			project: Type.Optional(Type.String({ description: "Project id (default: this directory's; a question about a time looks in every project)" })),
			all: Type.Optional(Type.Boolean({ description: "Search every project" })),
			since: Type.Optional(Type.String({ description: "YYYY-MM-DD: look at this time instead of the one the question names" })),
			until: Type.Optional(Type.String({ description: "YYYY-MM-DD, end of since (inclusive)" })),
		}), "ravnori_ask: answer a question about past work (why, what was done when) from the record, with sources"),
	);
	pi.registerTool(
		tool("timeline", "What happened around one memory (\"58645\") or transcript event (\"E123\"). Rarely needed: use when you must reconstruct a sequence (what led to a bug, what was tried before a fix).", Type.Object({
			anchor: Type.Union([Type.String(), Type.Number()], { description: "Memory id or E<id>" }),
			depth_before: Type.Optional(Type.Number()),
			depth_after: Type.Optional(Type.Number()),
		})),
	);
	pi.registerTool(
		tool("remember", "Pin a fact every agent should see at session start. Use only when the user asks to remember something.", Type.Object({
			fact: Type.String({ description: "The fact, stated so it stands alone" }),
			scope: Type.Optional(Type.String({ description: "project (default) or global" })),
		})),
	);
	pi.registerTool(
		tool("get_observations", "Full text of memories or transcript events: use when a memory you were shown or found looks relevant, since titles alone can mislead. Includes, when available, excerpts of the transcript evidence and whether the files a memory names changed since (no note does not mean unchanged).", Type.Object({
			ids: Type.Array(Type.Union([Type.String(), Type.Number()]), { description: "Ids, e.g. [58645, \"E72923\"]" }),
		}), "ravnori_get_observations: full text of a memory shown to you (#id) that looks relevant"),
	);
	pi.registerTool(
		tool("recall_file", "Memories about one file (past bugs, decisions, changes), one line each, marked with whether its own edits are still in the file (else whether the file changed since): use before changing a file you have not worked on in this session; once per file. Says so when there are none.", Type.Object({
			path: Type.String({ description: "The file, absolute or relative to the session's working directory" }),
			cwd: Type.Optional(Type.String({ description: "Directory a relative path starts from (default: the session's)" })),
			limit: Type.Optional(Type.Number({ description: "Max memories (default 5)" })),
		}), "ravnori_recall_file: past bugs and decisions about a file, before you change it"),
	);
}
"#;

fn watch_service(p: &Plan) -> Result<()> {
    say!("watch service");
    let m = crate::service::manager();
    let file = m.file();
    if p.dry_run {
        say!("  would write {} and start it", file.display());
        return Ok(());
    }
    std::fs::create_dir_all(file.parent().expect("has parent"))?;
    std::fs::write(&file, m.definition(&p.bin))?;
    if m.start() {
        say!("  started the watch service ({})", file.display());
    } else {
        let how = if m.can_start() {
            format!("start it with: {}", m.start_hint())
        } else {
            m.start_hint()
        };
        say!("  wrote {}; {how}", file.display());
    }
    Ok(())
}

/// Remove everything `install` added. Backs up each file first.
pub fn uninstall(dry_run: bool) -> Result<()> {
    let p = Plan {
        bin: String::new(),
        dry_run,
        claude: true,
        codex: true,
        pi: true,
        watch: false,
    };
    let mut files: Vec<(String, PathBuf)> = ingest::claude_config_dirs()
        .into_iter()
        .map(|d| {
            (
                format!("Claude Code ({})", d.display()),
                d.join("settings.json"),
            )
        })
        .collect();
    files.push(("Codex".into(), db::home().join(".codex/hooks.json")));
    for (name, path) in files {
        if !path.exists() {
            continue;
        }
        let mut doc = read_json(&path)?;
        let n = strip_ours(&mut doc);
        say!("{name}: {n} rvn hook(s) in {}", path.display());
        if n > 0 {
            write_json(&p, &path, &doc)?;
        }
    }
    for dir in ingest::claude_config_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
    {
        let removed = mcp_via_cli(
            &p,
            "claude",
            &claude_env(&dir),
            &["--version"],
            &["mcp", "remove", "--scope", "user", "ravnori"],
        )?;
        // Without the `claude` command: a state file holding only what ravnori created before
        // Claude Code was installed is ravnori's to delete; anything else is Claude Code's.
        let state = claude_state(&dir);
        if !removed && let Ok(doc) = read_json(&state) {
            if only_ravnori_state(&doc) {
                if dry_run {
                    say!("  would remove {}", state.display());
                } else {
                    backup(&state)?;
                    std::fs::remove_file(&state)?;
                    say!("  removed {} (ravnori had created it)", state.display());
                }
            } else if doc
                .get("mcpServers")
                .and_then(|m| m.get("ravnori"))
                .is_some()
            {
                say!(
                    "  mcp: still registered in {}; run: claude mcp remove --scope user ravnori",
                    state.display()
                );
            }
        }
    }
    let cfg = db::home().join(".codex/config.toml");
    if let Ok(cur) = std::fs::read_to_string(&cfg) {
        let stripped = remove_ravnori_server(&cur)
            .with_context(|| format!("{} is not valid TOML; left unchanged", cfg.display()))?;
        if stripped != cur {
            if dry_run {
                say!(
                    "  would remove [mcp_servers.ravnori] from {}",
                    cfg.display()
                );
            } else {
                backup(&cfg)?;
                std::fs::write(&cfg, stripped)?;
                say!("  removed [mcp_servers.ravnori] from {}", cfg.display());
            }
        }
    }
    let m = crate::service::manager();
    let unit = m.file();
    if unit.exists() {
        if dry_run {
            say!("  would stop and remove {}", unit.display());
        } else {
            m.stop();
            std::fs::remove_file(&unit)?;
            m.forget();
            say!("  removed {}", unit.display());
        }
    }
    let ext = db::home().join(".pi/agent/extensions/ravnori");
    if ext.exists() {
        if dry_run {
            say!("  would remove {}", ext.display());
        } else {
            std::fs::remove_dir_all(&ext)?;
            say!("  removed {}", ext.display());
        }
    }
    say!(
        "Data is kept in {} (delete it yourself if you want).",
        db::data_dir().display()
    );
    Ok(())
}

/// `config.toml` with `[mcp_servers.ravnori]` running `bin` (`args = ["mcp"]`): added when
/// missing, `command` and `args` corrected when they differ, every other key, comment and
/// table kept as it was (format-preserving edit). Unchanged text when already right.
fn set_ravnori_server(src: &str, bin: &str) -> Result<String> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};
    let mut doc: DocumentMut = src.parse()?;
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_like_mut()
        .context("mcp_servers is not a table")?;
    let right = servers.get("ravnori").is_some_and(|m| {
        m.get("command").and_then(|c| c.as_str()) == Some(bin)
            && m.get("args")
                .and_then(|a| a.as_array())
                .is_some_and(|a| a.len() == 1 && a.get(0).and_then(|v| v.as_str()) == Some("mcp"))
    });
    if right {
        return Ok(src.to_string());
    }
    let ravnori = servers
        .entry("ravnori")
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .context("mcp_servers.ravnori is not a table")?;
    let mut args = Array::new();
    args.push("mcp");
    // Only the value that differs changes, keeping the comments around it.
    set_keeping_decor(ravnori, "command", value(bin));
    set_keeping_decor(ravnori, "args", value(args));
    Ok(doc.to_string())
}

/// Set `key` to `new` unless it already equals it, keeping the old value's surrounding
/// whitespace and comments (and the key's own formatting, which setting in place keeps).
fn set_keeping_decor(t: &mut dyn toml_edit::TableLike, key: &str, mut new: toml_edit::Item) {
    match t.get_mut(key) {
        Some(old) if old.to_string().trim() == new.to_string().trim() => {}
        Some(old) => {
            if let (Some(o), Some(n)) = (old.as_value(), new.as_value_mut()) {
                *n.decor_mut() = o.decor().clone();
            }
            *old = new;
        }
        None => {
            t.insert(key, new);
        }
    }
}

/// `config.toml` without `[mcp_servers.ravnori]`; everything else as it was.
fn remove_ravnori_server(src: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = src.parse()?;
    let removed = doc
        .get_mut("mcp_servers")
        .and_then(|s| s.as_table_like_mut())
        .and_then(|s| s.remove("ravnori"))
        .is_some();
    Ok(if removed {
        doc.to_string()
    } else {
        src.to_string()
    })
}

pub fn default_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rvn".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_user_hooks_sharing_a_group() {
        let mut doc = json!({ "hooks": { "Stop": [
            { "hooks": [
                { "type": "command", "command": "notify-send done" },
                { "type": "command", "command": "/x/rvn hook claude stop" }
            ] },
            { "hooks": [{ "type": "command", "command": "/x/rvn hook claude stop" }] }
        ], "PreToolUse": [{ "matcher": "Read", "hooks": [{ "type": "command", "command": "guard" }] }] } });
        assert_eq!(strip_ours(&mut doc), 2);
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "notify-send done"
        );
        merge_hooks(&mut doc, &hook_entries("/x/rvn", "claude"), &json!({})).unwrap();
        merge_hooks(&mut doc, &hook_entries("/x/rvn", "claude"), &json!({})).unwrap();
        let stop = doc["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.len(),
            2,
            "user group + one ravnori group, even after two installs"
        );
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "guard"
        );
    }

    /// Re-running install keeps ravnori's hooks where they are (Codex trust is by position),
    /// and moves them only when they need changing.
    #[test]
    fn a_repeat_install_moves_no_hook() {
        let extra = json!({ "additionalContextLimit": 12000 });
        let mut doc = json!({ "hooks": { "SessionStart": [
            { "hooks": [{ "type": "command", "command": "gh-axi" }] }
        ] } });
        merge_hooks(&mut doc, &hook_entries("/x/rvn", "codex"), &extra).unwrap();
        // The user adds a hook after ravnori's.
        doc["hooks"]["SessionStart"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "hooks": [{ "type": "command", "command": "later" }] }));
        let before = doc.clone();
        merge_hooks(&mut doc, &hook_entries("/x/rvn", "codex"), &extra).unwrap();
        assert_eq!(doc, before, "nothing moved");
        // A moved binary is a real change: ravnori's hooks are replaced.
        merge_hooks(&mut doc, &hook_entries("/y/rvn", "codex"), &extra).unwrap();
        assert!(doc.to_string().contains("/y/rvn hook codex prompt"));
        assert!(!doc.to_string().contains("/x/rvn"));
        assert_eq!(
            doc["hooks"]["SessionStart"][1]["hooks"][0]["command"],
            "later"
        );
    }

    /// A duplicated hook does not stand in for a missing one: the set is repaired.
    #[test]
    fn duplicates_do_not_hide_a_missing_hook() {
        let entries = hook_entries("/x/rvn", "claude");
        let mut doc = json!({});
        merge_hooks(&mut doc, &entries, &json!({})).unwrap();
        assert!(ours_match(&doc, &entries, &json!({})));
        // PostToolUse gone, Stop twice: the same count, not the same set.
        let stop = doc["hooks"]["Stop"][0].clone();
        doc["hooks"]["Stop"].as_array_mut().unwrap().push(stop);
        doc["hooks"].as_object_mut().unwrap().remove("PostToolUse");
        assert!(!ours_match(&doc, &entries, &json!({})));
        merge_hooks(&mut doc, &entries, &json!({})).unwrap();
        assert!(ours_match(&doc, &entries, &json!({})));
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(doc["hooks"]["PostToolUse"].is_array());
    }

    #[test]
    fn codex_server_is_edited_structurally() {
        // Multi-line array, comments, a commented-out header and other keys all survive.
        let src = "# top\nmodel = \"x\"\n\n# [mcp_servers.ravnori] (old, commented)\n[mcp_servers.ravnori]\ncommand = \"/old/rvn\" # moved\nargs = [\n  \"mcp\",\n  \"--old\",\n]\nenv = { X = \"1\" }\n\n[mcp_servers.other]\ncommand = \"o\"\n";
        let out = set_ravnori_server(src, "/new/rvn").unwrap();
        let doc: toml_edit::DocumentMut = out.parse().expect("still valid TOML");
        assert_eq!(
            doc["mcp_servers"]["ravnori"]["command"].as_str(),
            Some("/new/rvn")
        );
        let args: Vec<&str> = doc["mcp_servers"]["ravnori"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(args, ["mcp"]);
        assert_eq!(
            doc["mcp_servers"]["ravnori"]["env"]["X"].as_str(),
            Some("1")
        );
        assert_eq!(doc["mcp_servers"]["other"]["command"].as_str(), Some("o"));
        assert!(out.starts_with("# top\nmodel = \"x\""), "{out}");
        assert!(
            out.contains("command = \"/new/rvn\" # moved"),
            "comment kept: {out}"
        );
        assert!(
            out.contains("# [mcp_servers.ravnori] (old, commented)"),
            "{out}"
        );
        assert_eq!(
            out.matches("[mcp_servers.ravnori]").count(),
            2,
            "one real, one comment: {out}"
        );
        // Already right: byte for byte the same.
        assert_eq!(set_ravnori_server(&out, "/new/rvn").unwrap(), out);
        // Missing: added; a file without it keeps its text.
        let added = set_ravnori_server("model = \"x\"\n", "/m").unwrap();
        assert!(added.starts_with("model = \"x\"\n"), "{added}");
        let d: toml_edit::DocumentMut = added.parse().unwrap();
        assert_eq!(d["mcp_servers"]["ravnori"]["command"].as_str(), Some("/m"));
        // Invalid TOML is refused, never rewritten.
        assert!(set_ravnori_server("[broken\n", "/m").is_err());
        // Removal takes only ravnori's table.
        let gone = remove_ravnori_server(&out).unwrap();
        let d: toml_edit::DocumentMut = gone.parse().unwrap();
        assert!(d["mcp_servers"].get("ravnori").is_none());
        assert_eq!(d["mcp_servers"]["other"]["command"].as_str(), Some("o"));
        assert_eq!(remove_ravnori_server("a = 1\n").unwrap(), "a = 1\n");
    }

    #[test]
    fn collected_reports_survive_a_panic_and_nest() {
        let (_, outer) = collecting(|| {
            say!("outer one");
            let (_, inner) = collecting(|| say!("inner"));
            assert_eq!(inner, ["inner"]);
            say!("outer two");
        });
        assert_eq!(outer, ["outer one", "outer two"]);
        let caught = std::panic::catch_unwind(|| collecting(|| panic!("boom")));
        assert!(caught.is_err());
        // Nothing is left collecting on this thread: the next report is not swallowed.
        assert!(LOG.with(|l| l.borrow().is_none()));
    }

    #[test]
    fn pi_extension_quotes_windows_paths() {
        let lit = serde_json::to_string(r"C:\bin\rvn.exe").unwrap();
        assert_eq!(lit, r#""C:\\bin\\rvn.exe""#);
    }
}
