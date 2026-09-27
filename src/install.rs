//! `mnem install`: wire mnem into Claude Code, Codex and pi.
//!
//! Every file is backed up before it is changed (`<file>.bak-mnem-<ts>`), entries are
//! idempotent (re-running replaces mnem's own entries and nothing else), and
//! `--dry-run` prints the plan without writing.

use crate::db;
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
}

/// Replace mnem's entries in a Claude-style `{"hooks": {Event: [group]}}` document.
fn merge_hooks(doc: &mut Value, entries: &[HookEntry], extra: &Value) {
    let root = doc.as_object_mut().expect("object");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().expect("hooks object");
    let ours = |g: &Value| {
        g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains("mnem hook "))
            })
        })
    };
    for (event, matcher, command, timeout) in entries {
        let groups = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let arr = groups.as_array_mut().expect("event array");
        arr.retain(|g| !ours(g));
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
    println!("Claude Code");
    let path = db::home().join(".claude/settings.json");
    let mut doc = read_json(&path)?;
    merge_hooks(&mut doc, &hook_entries(&p.bin, "claude"), &json!({}));
    for (event, _, cmd, _) in hook_entries(&p.bin, "claude") {
        println!("  hook {event}: {cmd}");
    }
    write_json(p, &path, &doc)?;
    mcp_via_cli(
        p,
        "claude",
        &["mcp", "remove", "--scope", "user", "mnem"],
        &["mcp", "add", "--scope", "user", "mnem", "--", &p.bin, "mcp"],
    )
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
    );
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

fn mcp_via_cli(p: &Plan, cli: &str, remove: &[&str], add: &[&str]) -> Result<()> {
    if p.dry_run {
        println!("  would run: {cli} {}", add.join(" "));
        return Ok(());
    }
    let _ = Command::new(cli).args(remove).output();
    match Command::new(cli).args(add).output() {
        Ok(o) if o.status.success() => println!("  mcp: {cli} {}", add.join(" ")),
        Ok(o) => println!(
            "  mcp: `{cli} {}` failed: {}",
            add.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!(
            "  mcp: {cli} not found ({e}); add manually: {cli} {}",
            add.join(" ")
        ),
    }
    Ok(())
}

fn pi(p: &Plan) -> Result<()> {
    println!("pi");
    let dir = db::home().join(".pi/agent/extensions/mnem");
    let path = dir.join("index.ts");
    let src = PI_EXTENSION.replace("__MNEM_BIN__", &p.bin);
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

const MNEM = process.env.MNEM_BIN ?? "__MNEM_BIN__";

export default function (pi: ExtensionAPI) {
	let session = "";
	let startContext = "";

	const run = async (args: string[]) => {
		try {
			const r = await pi.exec(MNEM, args);
			return r.code === 0 ? r.stdout : "";
		} catch {
			return "";
		}
	};

	pi.on("session_start", async (_event, ctx) => {
		session = `pi:${ctx.sessionManager.getSessionId()}`;
		const file = ctx.sessionManager.getSessionFile();
		if (file) await run(["ingest", file]);
		startContext = await run(["context", "--cwd", ctx.cwd, "--session", session]);
	});

	pi.on("before_agent_start", async (event, ctx) => {
		const file = ctx.sessionManager.getSessionFile();
		if (file) await run(["ingest", file]);
		const delta = session ? await run(["delta", "--session", session, "--cwd", ctx.cwd]) : "";
		const add = [startContext, delta].filter((s) => s.trim()).join("\n\n");
		if (add) return { systemPrompt: `${event.systemPrompt}\n\n${add}` };
	});

	const tool = (name: string, description: string, parameters: any) =>
		defineTool({
			name: `mnem_${name}`,
			label: `mnem ${name}`,
			description,
			parameters,
			async execute(_id, params) {
				const text = await run(["tool", name, JSON.stringify(params ?? {})]);
				return { content: [{ type: "text", text: text || "No results." }], details: {} };
			},
		});

	pi.registerTool(
		tool("search", "Search long-term memory (all agents, all projects). Returns an index with ids; then use mnem_get_observations.", Type.Object({
			query: Type.String({ description: "Search query" }),
			project: Type.Optional(Type.String({ description: "Filter by project (substring)" })),
			type: Type.Optional(Type.String({ description: "observations | sessions | prompts | events" })),
			limit: Type.Optional(Type.Number({ description: "Max results (default 20)" })),
		})),
	);
	pi.registerTool(
		tool("timeline", "Show what happened around one memory id (number) or event id (\"E123\").", Type.Object({
			anchor: Type.String({ description: "Observation id or E<id>" }),
			depth_before: Type.Optional(Type.Number()),
			depth_after: Type.Optional(Type.Number()),
		})),
	);
	pi.registerTool(
		tool("get_observations", "Full details for ids returned by mnem_search or mnem_timeline.", Type.Object({
			ids: Type.Array(Type.String(), { description: "Ids, e.g. [\"58645\", \"E72923\"]" }),
		})),
	);
}
"#;

pub fn default_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mnem".into())
}
