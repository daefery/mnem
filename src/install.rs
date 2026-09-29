//! `mnem install`: wire mnem into Claude Code, Codex and pi.
//!
//! Every file is backed up before it is changed (`<file>.bak-mnem-<ts>`), entries are
//! idempotent (re-running replaces mnem's own entries and nothing else), and
//! `--dry-run` prints the plan without writing.

use crate::db;
use crate::ingest;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Plan {
    pub bin: String,
    pub dry_run: bool,
    pub claude: bool,
    pub codex: bool,
    pub pi: bool,
    pub watch: bool,
}

pub fn run(p: &Plan) -> Result<()> {
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
    if p.dry_run {
        println!("\n(dry run: nothing written)");
    }
    Ok(())
}

type HookEntry = (&'static str, Option<&'static str>, String, u64);

fn hook_entries(bin: &str, agent: &str) -> Vec<HookEntry> {
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
        .is_some_and(|c| c.contains("mnem hook "))
}

/// Remove mnem's hooks everywhere. Other hooks stay, even when they share a group with
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

/// Replace mnem's entries in a Claude-style `{"hooks": {Event: [group]}}` document.
fn merge_hooks(doc: &mut Value, entries: &[HookEntry], extra: &Value) -> Result<()> {
    anyhow::ensure!(doc.is_object(), "settings root is not a JSON object");
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

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))
        }
        _ => Ok(json!({})),
    }
}

fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let b = PathBuf::from(format!("{}.bak-mnem-{}", path.display(), db::now_ms()));
        std::fs::copy(path, &b)?;
        println!("  backup: {}", b.display());
    }
    Ok(())
}

fn write_json(p: &Plan, path: &Path, doc: &Value) -> Result<()> {
    let s = serde_json::to_string_pretty(doc)? + "\n";
    if p.dry_run {
        println!("  would write {} ({} bytes)", path.display(), s.len());
        return Ok(());
    }
    backup(path)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, s)?;
    println!("  wrote {}", path.display());
    Ok(())
}

fn claude(p: &Plan) -> Result<()> {
    for dir in ingest::claude_config_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
    {
        println!("Claude Code ({})", dir.display());
        let path = dir.join("settings.json");
        let mut doc = read_json(&path)?;
        merge_hooks(&mut doc, &hook_entries(&p.bin, "claude"), &json!({}))?;
        for (event, _, cmd, _) in hook_entries(&p.bin, "claude") {
            println!("  hook {event}: {cmd}");
        }
        write_json(p, &path, &doc)?;
        mcp_via_cli(
            p,
            "claude",
            &claude_env(&dir),
            &["mcp", "remove", "--scope", "user", "mnem"],
            &["mcp", "add", "--scope", "user", "mnem", "--", &p.bin, "mcp"],
        )?;
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
    println!("Codex");
    let path = db::home().join(".codex/hooks.json");
    let mut doc = read_json(&path)?;
    // Codex caps injected context per hook; mnem's session context is ~8 KB.
    merge_hooks(
        &mut doc,
        &hook_entries(&p.bin, "codex"),
        &json!({ "additionalContextLimit": 12000 }),
    )?;
    for (event, _, cmd, _) in hook_entries(&p.bin, "codex") {
        println!("  hook {event}: {cmd}");
    }
    write_json(p, &path, &doc)?;
    // MCP server: append a [mcp_servers.mnem] table unless one exists.
    let cfg = db::home().join(".codex/config.toml");
    let cur = std::fs::read_to_string(&cfg).unwrap_or_default();
    if cur.contains("[mcp_servers.mnem]") {
        println!("  mcp: [mcp_servers.mnem] already in {}", cfg.display());
    } else {
        let block = format!(
            "\n[mcp_servers.mnem]\ncommand = {:?}\nargs = [\"mcp\"]\n",
            p.bin
        );
        if p.dry_run {
            println!("  would append to {}:{block}", cfg.display());
        } else {
            backup(&cfg)?;
            std::fs::write(&cfg, cur + &block)?;
            println!("  mcp: added [mcp_servers.mnem] to {}", cfg.display());
        }
    }
    Ok(())
}

fn mcp_via_cli(
    p: &Plan,
    cli: &str,
    env: &[(String, String)],
    remove: &[&str],
    add: &[&str],
) -> Result<()> {
    let shown = format!(
        "{}{cli} {}",
        env.iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect::<String>(),
        add.join(" ")
    );
    if p.dry_run {
        println!("  would run: {shown}");
        return Ok(());
    }
    let run = |args: &[&str]| {
        let mut c = Command::new(cli);
        c.args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output()
    };
    let _ = run(remove);
    match run(add) {
        Ok(o) if o.status.success() => println!("  mcp: {shown}"),
        Ok(o) => println!(
            "  mcp: `{shown}` failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("  mcp: {cli} not found ({e}); run manually: {shown}"),
    }
    Ok(())
}

fn pi(p: &Plan) -> Result<()> {
    println!("pi");
    let dir = db::home().join(".pi/agent/extensions/mnem");
    let path = dir.join("index.ts");
    // JSON string encoding is a valid TS string literal (escapes Windows backslashes).
    let src = PI_EXTENSION.replace("__MNEM_BIN__", &serde_json::to_string(&p.bin)?);
    if p.dry_run {
        println!("  would write {} ({} bytes)", path.display(), src.len());
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    backup(&path)?;
    std::fs::write(&path, src)?;
    println!("  wrote {}", path.display());
    Ok(())
}

const PI_EXTENSION: &str = r#"/**
 * mnem for pi: session-start context, cross-agent updates, and memory tools.
 * Generated by `mnem install`; re-run it to update. Capture itself needs nothing
 * here: mnem reads pi's own session files.
 */
import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { homedir } from "node:os";
import { resolve } from "node:path";

const MNEM = process.env.MNEM_BIN ?? __MNEM_BIN__;

export default function (pi: ExtensionAPI) {
	let session = "";
	// Files already looked up in this session: no process for a second read of one.
	let touched = new Set<string>();
	let startContext = "";
	let delivered = false;

	const run = async (args: string[]) => {
		try {
			const r = await pi.exec(MNEM, args);
			return r.code === 0 ? r.stdout.trim() : "";
		} catch {
			return "";
		}
	};

	// Tools the agent calls: failures are reported, not passed off as "no results", and
	// relative paths start from the session's current directory.
	const call = async (args: string[], signal: AbortSignal | undefined, cwd: string) => {
		const r = await pi.exec(MNEM, args, { signal, cwd });
		if (r.code !== 0) throw new Error(`mnem failed (exit ${r.code}): ${(r.stderr || r.stdout).trim().slice(0, 500)}`);
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
			return { message: { customType: "mnem-context", content, display: false } };
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
			const r = await pi.exec(MNEM, ["file", abs, "--touch", "--session", session, "--cwd", ctx.cwd], {
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
			name: `mnem_${name}`,
			label: `mnem ${name}`,
			description,
			promptSnippet,
			parameters,
			async execute(_id, params, signal, _onUpdate, ctx) {
				const args: any = { ...(params ?? {}) };
				if (name === "recall_file") {
					if (!args.cwd) args.cwd = ctx.cwd;
					if (session) args.session = session;
				}
				const text = await call(["tool", name, JSON.stringify(args)], signal, ctx.cwd);
				return { content: [{ type: "text", text: text || "No results." }], details: {} };
			},
		});

	pi.registerTool(
		tool("search", "Search past work across all agents (decisions, bugs, fixes, what was tried): use when the user refers to earlier work (\"like last time\", \"that bug\", \"why did we\"), or when unsure whether a design question was already settled in this project (pass project). Not for general programming knowledge. Returns a one-line-per-hit index with ids for mnem_get_observations.", Type.Object({
			query: Type.String({ description: "Search query" }),
			project: Type.Optional(Type.String({ description: "Filter by project (substring)" })),
			type: Type.Optional(Type.String({ description: "observations | sessions | prompts | events" })),
			limit: Type.Optional(Type.Number({ description: "Max results (default 20)" })),
		}), "mnem_search: search past work (decisions, bugs, fixes) when the user refers to earlier work"),
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
		}), "mnem_get_observations: full text of a memory shown to you (#id) that looks relevant"),
	);
	pi.registerTool(
		tool("recall_file", "Memories about one file (past bugs, decisions, changes), one line each, marked with whether its own edits are still in the file (else whether the file changed since): use before changing a file you have not worked on in this session; once per file. Says so when there are none.", Type.Object({
			path: Type.String({ description: "The file, absolute or relative to the session's working directory" }),
			cwd: Type.Optional(Type.String({ description: "Directory a relative path starts from (default: the session's)" })),
			limit: Type.Optional(Type.Number({ description: "Max memories (default 5)" })),
		}), "mnem_recall_file: past bugs and decisions about a file, before you change it"),
	);
}
"#;

fn watch_service(p: &Plan) -> Result<()> {
    println!("watch service");
    let unit = db::home().join(".config/systemd/user/mnem-watch.service");
    let body = format!(
        "[Unit]\nDescription=mnem transcript watcher\n\n[Service]\nExecStart={} watch\nRestart=always\nRestartSec=10\nNice=10\n\n[Install]\nWantedBy=default.target\n",
        p.bin
    );
    if p.dry_run {
        println!("  would write {} and enable it", unit.display());
        return Ok(());
    }
    std::fs::create_dir_all(unit.parent().expect("has parent"))?;
    std::fs::write(&unit, body)?;
    let ok = |args: &[&str]| {
        Command::new("systemctl")
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if ok(&["--user", "daemon-reload"]) && ok(&["--user", "enable", "--now", "mnem-watch.service"])
    {
        println!("  enabled and started mnem-watch.service");
    } else {
        println!(
            "  wrote {}; start it with: systemctl --user enable --now mnem-watch.service",
            unit.display()
        );
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
        println!("{name}: {n} mnem hook(s) in {}", path.display());
        if n > 0 {
            write_json(&p, &path, &doc)?;
        }
    }
    for dir in ingest::claude_config_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
    {
        mcp_via_cli(
            &p,
            "claude",
            &claude_env(&dir),
            &["--version"],
            &["mcp", "remove", "--scope", "user", "mnem"],
        )?;
    }
    let cfg = db::home().join(".codex/config.toml");
    if let Ok(cur) = std::fs::read_to_string(&cfg) {
        let stripped = remove_toml_table(&cur, "mcp_servers.mnem");
        if stripped != cur {
            if dry_run {
                println!("  would remove [mcp_servers.mnem] from {}", cfg.display());
            } else {
                backup(&cfg)?;
                std::fs::write(&cfg, stripped)?;
                println!("  removed [mcp_servers.mnem] from {}", cfg.display());
            }
        }
    }
    let unit = db::home().join(".config/systemd/user/mnem-watch.service");
    if unit.exists() {
        if dry_run {
            println!("  would stop and remove {}", unit.display());
        } else {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", "mnem-watch.service"])
                .output();
            std::fs::remove_file(&unit)?;
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();
            println!("  removed {}", unit.display());
        }
    }
    let ext = db::home().join(".pi/agent/extensions/mnem");
    if ext.exists() {
        if dry_run {
            println!("  would remove {}", ext.display());
        } else {
            std::fs::remove_dir_all(&ext)?;
            println!("  removed {}", ext.display());
        }
    }
    println!(
        "Data is kept in {} (delete it yourself if you want).",
        db::data_dir().display()
    );
    Ok(())
}

/// Drop `[name]` and its keys (up to the next table header) from a TOML document.
fn remove_toml_table(src: &str, name: &str) -> String {
    let header = format!("[{name}]");
    let mut out = Vec::new();
    let mut skipping = false;
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            skipping = t == header || t.starts_with(&format!("[{name}."));
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut s = out.join("\n");
    if src.ends_with('\n') {
        s.push('\n');
    }
    s
}

pub fn default_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mnem".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_user_hooks_sharing_a_group() {
        let mut doc = json!({ "hooks": { "Stop": [
            { "hooks": [
                { "type": "command", "command": "notify-send done" },
                { "type": "command", "command": "/x/mnem hook claude stop" }
            ] },
            { "hooks": [{ "type": "command", "command": "/x/mnem hook claude stop" }] }
        ], "PreToolUse": [{ "matcher": "Read", "hooks": [{ "type": "command", "command": "guard" }] }] } });
        assert_eq!(strip_ours(&mut doc), 2);
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "notify-send done"
        );
        merge_hooks(&mut doc, &hook_entries("/x/mnem", "claude"), &json!({})).unwrap();
        merge_hooks(&mut doc, &hook_entries("/x/mnem", "claude"), &json!({})).unwrap();
        let stop = doc["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.len(),
            2,
            "user group + one mnem group, even after two installs"
        );
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "guard"
        );
    }

    #[test]
    fn removes_toml_table_only() {
        let src = "a = 1\n[mcp_servers.x]\ncommand = \"x\"\n[mcp_servers.mnem]\ncommand = \"m\"\nargs = [\"mcp\"]\n[z]\nk = 2\n";
        assert_eq!(
            remove_toml_table(src, "mcp_servers.mnem"),
            "a = 1\n[mcp_servers.x]\ncommand = \"x\"\n[z]\nk = 2\n"
        );
    }

    #[test]
    fn pi_extension_quotes_windows_paths() {
        let lit = serde_json::to_string(r"C:\bin\mnem.exe").unwrap();
        assert_eq!(lit, r#""C:\\bin\\mnem.exe""#);
    }
}
