//! MCP server over stdio (JSON-RPC 2.0, one message per line).
//!
//! Tool names and parameters follow claude-mem (`search`, `timeline`,
//! `get_observations`, `session_start_context`) so existing prompts and skills keep
//! working. Memory ids are numbers (`58645`); raw transcript events are `E<id>`.

use crate::context;
use crate::hook;
use crate::search::fts_query;
use crate::text;
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, ToSql, params_from_iter};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const INSTRUCTIONS: &str = "mnem is this machine's memory of past work: decisions, bugs, fixes and what was tried, \
captured from every Claude Code, Codex and pi session. Use it when:
- the user points at earlier work (\"like last time\", \"that bug\", \"why did we\", \"what did we decide\"), or \
you are unsure whether a design question was already settled in this project: search(query, project). Not for \
general programming knowledge or for what this conversation already shows; refine a search instead of repeating it;
- a memory shown to you (at session start, with a prompt, or when a file was opened) looks relevant to the task: \
get_observations([ids]) gives its full text and, when available, the transcript evidence behind it; titles alone \
can mislead;
- you are about to change a file and no memories about it were shown in this session (Claude Code shows them \
when a file is first opened): recall_file(path), once per file.
Numeric ids are memories; ids like \"E123\" are raw transcript events. \
Call remember(fact) only when the user asks you to remember something.";

/// Set when this server stays out of the way of another registration of mnem's tools.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Protocol versions this server implements, newest first.
const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve(conn: &Connection) -> Result<()> {
    serve_with(conn, false)
}

/// Serve; with `quiet`, list no tools (another registration of mnem already offers them).
pub fn serve_with(conn: &Connection, quiet: bool) -> Result<()> {
    QUIET.store(quiet, std::sync::atomic::Ordering::Relaxed);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle(conn, &line) {
            write_msg(&mut out, &resp)?;
        }
    }
    Ok(())
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// One JSON-RPC message in, at most one response out (notifications get none).
pub fn handle(conn: &Connection, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(rpc_error(Value::Null, -32700, format!("parse error: {e}"))),
    };
    let id = msg.get("id").cloned();
    let valid_id = matches!(id, Some(Value::String(_)) | Some(Value::Number(_)));
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || (id.is_some() && !valid_id) {
        return Some(rpc_error(
            if valid_id {
                id.unwrap_or(Value::Null)
            } else {
                Value::Null
            },
            -32600,
            "invalid request",
        ));
    }
    let id = id?; // notification
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(id, -32600, "missing method"));
    };
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked
                .filter(|v| SUPPORTED.contains(v))
                .unwrap_or(SUPPORTED[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mnem", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" if QUIET.load(std::sync::atomic::Ordering::Relaxed) => json!({ "tools": [] }),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if let Err(e) = validate(name, &args) {
                return Some(rpc_error(id, -32602, e));
            }
            match call(conn, name, &args) {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(e) => {
                    json!({ "content": [{ "type": "text", "text": format!("error: {e:#}") }], "isError": true })
                }
            }
        }
        _ => return Some(rpc_error(id, -32601, format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Check arguments against the advertised input schemas.
fn validate(name: &str, a: &Value) -> std::result::Result<(), String> {
    if !a.is_object() {
        return Err("arguments must be an object".into());
    }
    let strings: &[&str] = &[
        "path",
        "query",
        "project",
        "platformSource",
        "type",
        "obs_type",
        "dateStart",
        "dateEnd",
        "orderBy",
        "cwd",
    ];
    let numbers: &[&str] = &["limit", "offset", "depth_before", "depth_after"];
    let known: &[&str] = match name {
        "search" => &[
            "query",
            "limit",
            "project",
            "platformSource",
            "type",
            "obs_type",
            "dateStart",
            "dateEnd",
            "offset",
            "orderBy",
        ],
        "timeline" => &["anchor", "query", "depth_before", "depth_after", "project"],
        "get_observations" => &["ids", "limit", "orderBy", "project"],
        "session_start_context" => &["project", "cwd"],
        "recall_file" => &["path", "cwd", "limit", "session"],
        "remember" => &["fact", "scope", "project"],
        _ => return Err(format!("unknown tool: {name}")),
    };
    for (k, v) in a.as_object().into_iter().flatten() {
        if !known.contains(&k.as_str()) || v.is_null() {
            continue; // tolerate extra or null fields, as claude-mem did
        }
        if strings.contains(&k.as_str()) && !v.is_string() {
            return Err(format!("{k} must be a string"));
        }
        if numbers.contains(&k.as_str()) {
            let n = v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()));
            match n {
                Some(n) if n >= 0.0 && (k != "limit" || n >= 1.0) => {}
                _ => return Err(format!("{k} must be a non-negative number")),
            }
        }
        if k == "anchor" && !(v.is_number() || v.is_string()) {
            return Err("anchor must be a number or \"E<id>\"".into());
        }
    }
    if name == "remember" && str_arg(a, "fact").is_none() {
        return Err("fact (string) is required".into());
    }
    if name == "recall_file" && str_arg(a, "path").is_none() {
        return Err("path (string) is required".into());
    }
    if name == "get_observations" && !a.get("ids").is_some_and(Value::is_array) {
        return Err("ids (array) is required".into());
    }
    Ok(())
}

fn write_msg(out: &mut impl Write, v: &Value) -> Result<()> {
    writeln!(out, "{v}")?;
    out.flush()?;
    Ok(())
}

fn tools() -> Value {
    json!([
        {
            "name": "search",
            "description": "Search past work across all agents (decisions, bugs, fixes, what was tried): use when the user refers to earlier work, or when unsure whether a design question was already settled in this project (pass project). Not for general programming knowledge. Words or a plain question; returns a one-line-per-hit index with ids for get_observations.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "Words or a plain-language question (empty: most recent). Relevance order also matches meaning when mnem-watch runs" },
                "limit": { "type": "number", "description": "Max results (default 20)" },
                "project": { "type": "string", "description": "Filter by project (substring, e.g. repo name)" },
                "platformSource": { "type": "string", "description": "Filter by agent: claude, codex or pi" },
                "type": { "type": "string", "description": "'observations', 'sessions' (summaries), 'prompts', 'events' (raw transcript) or omit for observations+events" },
                "obs_type": { "type": "string", "description": "Observation types, comma-separated (bugfix, feature, decision, ...)" },
                "dateStart": { "type": "string", "description": "ISO date lower bound" },
                "dateEnd": { "type": "string", "description": "ISO date upper bound" },
                "offset": { "type": "number", "description": "Pagination offset" },
                "orderBy": { "type": "string", "description": "relevance (default), date_desc or date_asc" }
            }}
        },
        {
            "name": "timeline",
            "description": "What happened around a memory or transcript event (the steps before and after it). Rarely needed: use when you must reconstruct a sequence (what led to a bug, what was tried before a fix). anchor is an id or \"E<id>\".",
            "inputSchema": { "type": "object", "properties": {
                "anchor": { "type": ["number", "string"], "description": "Observation id or \"E<id>\"" },
                "query": { "type": "string", "description": "Find the anchor with a search instead" },
                "depth_before": { "type": "number", "description": "Items before (default 3)" },
                "depth_after": { "type": "number", "description": "Items after (default 3)" },
                "project": { "type": "string", "description": "Filter by project" }
            }}
        },
        {
            "name": "get_observations",
            "description": "Full text of memories or transcript events: use when a memory you were shown or found looks relevant, since titles alone can mislead. Includes, when available, excerpts of the transcript evidence and whether the files a memory names changed since (no note does not mean unchanged). ids are numbers or \"E<id>\".",
            "inputSchema": { "type": "object", "required": ["ids"], "properties": {
                "ids": { "type": "array", "items": { "type": ["number", "string"] }, "description": "Ids to fetch" },
                "limit": { "type": "number", "description": "Max items" }
            }}
        },
        {
            "name": "remember",
            "description": "Pin a fact the user wants every agent (Claude Code, Codex, pi) to see at session start. Use only when the user asks to remember something.",
            "inputSchema": { "type": "object", "required": ["fact"], "properties": {
                "fact": { "type": "string", "description": "The fact, stated so it stands alone" },
                "scope": { "type": "string", "description": "'project' (default) or 'global'" },
                "project": { "type": "string", "description": "Project id (default: this server's working directory)" }
            }}
        },
        {
            "name": "recall_file",
            "description": "Memories about one file: past bugs, decisions and changes that read or modified it, one line each, marked with whether the lines each memory's session wrote are still in the file, else whether the file changed since (commits, uncommitted edits, lines). Use before changing a file when no memories about it were shown in this session; once per file. Says so when there are none.",
            "inputSchema": { "type": "object", "required": ["path"], "properties": {
                "path": { "type": "string", "description": "The file, absolute or relative to cwd" },
                "cwd": { "type": "string", "description": "Directory relative paths start from (default: this server's working directory)" },
                "limit": { "type": "number", "description": "Max memories (default 5)" },
                "session": { "type": "string", "description": "Caller's mnem session id, if known: the file then counts as seen, so its memories are not shown again when it is opened" }
            }}
        },
        {
            "name": "session_start_context",
            "description": "The context mnem injects at session start for a project (recent sessions across agents, last summary, observations). Use only if no mnem context appeared at the start of this session.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": "Project id (default: this server's working directory)" },
                "cwd": { "type": "string", "description": "Resolve the project from this directory" }
            }}
        }
    ])
}

pub fn call(conn: &Connection, name: &str, a: &Value) -> Result<String> {
    // Uptake: which tool, for which project, and which memories it asked for.
    let ids: Vec<i64> = match name {
        "get_observations" => a["ids"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|x| match parse_id(x) {
                        Some(Id::Memory(n)) => Some(n),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "timeline" => match a.get("anchor").and_then(parse_id) {
            Some(Id::Memory(n)) => vec![n],
            _ => vec![],
        },
        _ => vec![],
    };
    let project = str_arg(a, "project").map(str::to_string).or_else(|| {
        let cwd = str_arg(a, "cwd").map(str::to_string).or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.to_string_lossy().into_owned())
        })?;
        crate::project::Resolver::default().resolve(Some(&cwd), None)
    });
    let _ = crate::uptake::mcp_call(conn, name, project.as_deref(), &ids);
    match name {
        "search" => search(conn, a),
        "timeline" => timeline(conn, a),
        "get_observations" => get(conn, a),
        "recall_file" => recall_file(conn, a),
        "remember" => {
            let fact = str_arg(a, "fact").ok_or_else(|| anyhow::anyhow!("fact is required"))?;
            let project = if str_arg(a, "scope") == Some("global") {
                None
            } else {
                let cwd = std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                Some(
                    str_arg(a, "project")
                        .map(str::to_string)
                        .or_else(|| hook::project_for(conn, None, cwd.as_deref()))
                        .ok_or_else(|| {
                            anyhow::anyhow!("cannot resolve project; pass project or scope=global")
                        })?,
                )
            };
            let id = crate::forget::remember(conn, fact, project.as_deref())?;
            Ok(format!(
                "Pinned #{id} for {}.",
                project.as_deref().unwrap_or("every project")
            ))
        }
        "session_start_context" => {
            let cwd = str_arg(a, "cwd").map(str::to_string).or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            let project = str_arg(a, "project")
                .map(str::to_string)
                .or_else(|| hook::project_for(conn, None, cwd.as_deref()))
                .ok_or_else(|| anyhow::anyhow!("cannot resolve project; pass project"))?;
            context::build(
                conn,
                &context::Options {
                    project: &project,
                    current: None,
                    budget_chars: 8000,
                    sessions: 5,
                    turns: 3,
                    observations: 30,
                },
            )
        }
        _ => bail!("unknown tool {name}"),
    }
}

fn str_arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn num_arg(a: &Value, k: &str, default: i64) -> i64 {
    a.get(k)
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(default)
}

fn date_arg(a: &Value, k: &str) -> Option<i64> {
    let s = str_arg(a, k)?;
    text::parse_ts(s).or_else(|| text::parse_ts(&format!("{s}T00:00:00Z")))
}

fn day(ms: i64) -> String {
    // yyyy-mm-dd from epoch millis (civil-from-days, Hinnant).
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

struct Filters {
    project: Option<String>,
    agent: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
}

const BEYOND_POOL: &str =
    "Relevance order covers the best 1000 memories; page further with orderBy date_desc.";

fn search(conn: &Connection, a: &Value) -> Result<String> {
    let raw = str_arg(a, "query").unwrap_or_default().trim();
    let q = fts_query(raw);
    let limit = num_arg(a, "limit", 20).clamp(1, 200);
    let offset = num_arg(a, "offset", 0).max(0);
    let f = Filters {
        project: str_arg(a, "project").map(str::to_string),
        agent: str_arg(a, "platformSource").map(str::to_string),
        start: date_arg(a, "dateStart"),
        end: date_arg(a, "dateEnd"),
    };
    let order = str_arg(a, "orderBy").unwrap_or("relevance");
    let mut kind = str_arg(a, "type").map(str::to_string);
    let mut obs_types: Vec<String> = str_arg(a, "obs_type")
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // claude-mem: any other `type` value is an observation-type filter.
    if let Some(k) = &kind
        && !["observations", "sessions", "prompts", "events"].contains(&k.as_str())
    {
        obs_types.push(k.clone());
        kind = Some("observations".into());
    }
    let want_mem = matches!(
        kind.as_deref(),
        None | Some("observations") | Some("sessions")
    );
    let want_ev = matches!(kind.as_deref(), None | Some("prompts") | Some("events"));
    let relevance = order == "relevance" && !raw.is_empty();
    // Both tables: take the first offset+limit of each, merge, then cut one page.
    let both = want_mem && want_ev;
    let (window, skip) = if both {
        (offset + limit, 0)
    } else {
        (limit, offset)
    };
    let mut notes = Vec::new();
    let mut mem: Vec<(i64, String)> = Vec::new();
    if want_mem {
        let mem_kind = match kind.as_deref() {
            Some("sessions") => Some("summary"),
            Some("observations") => Some("observation"),
            _ => None,
        };
        // Relevance order also asks the watch service for a query vector, so memories
        // that mean the same thing in other words rank too (keyword search otherwise).
        let vq = (relevance && crate::recall::semantic_enabled())
            .then(|| crate::embed::query_from_service(conn, raw))
            .flatten();
        let (rows, meaning) = search_memories(
            conn,
            raw,
            vq.as_ref(),
            &f,
            mem_kind,
            &obs_types,
            order,
            window,
            skip,
        )?;
        if meaning {
            notes.push(
                "Memories ranked by words and meaning; \"(by meaning)\" marks ones with none of the words.",
            );
        }
        if relevance && (offset + limit) as usize > crate::search::POOL {
            notes.push(BEYOND_POOL);
        }
        mem = rows;
    }
    let mut ev: Vec<(i64, String)> = Vec::new();
    if want_ev {
        let kinds: &[&str] = if kind.as_deref() == Some("prompts") {
            &["prompt"]
        } else {
            &[
                "prompt",
                "assistant",
                "error",
                "file_edit",
                "command",
                "recap",
                "compaction",
            ]
        };
        ev = search_events(conn, &q, &f, kinds, order, window, skip)?;
    }
    let mut out: Vec<(i64, String)> = if both && !relevance {
        let mut all: Vec<(i64, String)> = mem.into_iter().chain(ev).collect();
        if order == "date_asc" {
            all.sort_by_key(|r| r.0);
        } else {
            all.sort_by_key(|r| std::cmp::Reverse(r.0));
        }
        all
    } else {
        mem.into_iter().chain(ev).collect()
    };
    if both {
        out = out
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
    }
    if out.is_empty() {
        return Ok(if notes.contains(&BEYOND_POOL) {
            BEYOND_POOL.to_string()
        } else {
            "No results.".into()
        });
    }
    let mut text: Vec<String> = notes.iter().map(|n| n.to_string()).collect();
    let mut divided = false;
    for (_, line) in out {
        // Relevance lists memories first; mark where transcript events begin.
        if relevance && both && !divided && line.starts_with('E') {
            text.push("Transcript events:".into());
            divided = true;
        }
        text.push(line);
    }
    Ok(format!(
        "{}\n\nNext: timeline(anchor) or get_observations([ids]).",
        text.join("\n")
    ))
}

/// One page of memories as (created_at, index line), and whether meaning took part.
#[allow(clippy::too_many_arguments)]
fn search_memories(
    conn: &Connection,
    raw: &str,
    vq: Option<&crate::embed::Query>,
    f: &Filters,
    kind: Option<&str>,
    types: &[String],
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<(i64, String)>, bool)> {
    // Filters shared by the keyword and the vector ranking. Sensitive memories (personal
    // details) are left out unless asked for by type.
    let filters = || {
        let mut wh: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        push_common(
            &mut wh,
            &mut args,
            f,
            "m.project",
            "m.created_at",
            "m.session_id",
        );
        if let Some(k) = kind {
            wh.push("m.kind = ?".into());
            args.push(Box::new(k.to_string()));
        }
        if types.is_empty() {
            wh.push("coalesce(m.type, '') != 'sensitive'".into());
        } else {
            wh.push(format!("m.type IN ({})", vec!["?"; types.len()].join(",")));
            for t in types {
                args.push(Box::new(t.clone()));
            }
        }
        (wh.join(" AND "), args)
    };
    let (filter, _) = filters();
    if order == "relevance" && !raw.is_empty() {
        let ranked = crate::search::rank_memories(conn, raw, vq, &filter, &|| filters().1)?;
        let meaning = ranked.iter().any(|r| r.by_meaning);
        let page: Vec<_> = ranked
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        let mut out = Vec::new();
        for r in page {
            let (at, mut line) = memory_line(conn, r.id)?;
            if !r.by_words {
                line.push_str(" (by meaning)");
            }
            out.push((at, line));
        }
        return Ok((out, meaning));
    }
    // Date order (or no query): every word must match, newest or oldest first.
    let q = fts_query(raw);
    let (filter, mut args) = filters();
    let mut sql = String::from("SELECT m.id FROM memories m");
    let mut wh = vec![filter];
    if !q.is_empty() {
        sql.push_str(" JOIN memories_fts ON memories_fts.rowid = m.id");
        wh.insert(0, "memories_fts MATCH ?".into());
        args.insert(0, Box::new(q));
    }
    sql.push_str(&format!(" WHERE {}", wh.join(" AND ")));
    // Ties in time oldest id first, as the plan without the time index returned them.
    sql.push_str(if order == "date_asc" {
        " ORDER BY m.created_at ASC, m.id ASC"
    } else {
        " ORDER BY m.created_at DESC, m.id ASC"
    });
    sql.push_str(" LIMIT ? OFFSET ?");
    args.push(Box::new(limit));
    args.push(Box::new(offset));
    let mut st = conn.prepare(&sql)?;
    let ids: Vec<i64> = st
        .query_map(params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
            r.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for id in ids {
        out.push(memory_line(conn, id)?);
    }
    Ok((out, false))
}

/// (created_at, index line) for a memory.
fn memory_line(conn: &Connection, id: i64) -> Result<(i64, String)> {
    let mut st = conn.prepare_cached(
        "SELECT m.kind, coalesce(m.type, ''), coalesce(m.title, ''), coalesce(m.created_at, 0), coalesce(m.project, ''),
                EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = m.session_id)
         FROM memories m WHERE m.id = ?1",
    )?;
    let (kind, ty, title, at, project, scripted): (String, String, String, i64, String, bool) = st
        .query_row([id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
    let label = if kind == "summary" {
        "summary".to_string()
    } else {
        ty
    };
    Ok((
        at,
        format!(
            "#{id} [{label}] {} · {} · {}{}",
            day(at),
            short(&project),
            text::head(&title, 140),
            if scripted { " · scripted" } else { "" }
        ),
    ))
}

fn search_events(
    conn: &Connection,
    q: &str,
    f: &Filters,
    kinds: &[&str],
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<(i64, String)>> {
    let mut sql = String::from(
        "SELECT e.id, e.kind, coalesce(e.ts, 0), s.agent, coalesce(s.project, ''), coalesce(e.path, ''), coalesce(e.text, '')
         FROM events e JOIN sessions s ON s.id = e.session_id",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut wh: Vec<String> = vec![
        "e.thread IS NULL".into(),
        "(e.label IS NULL OR e.kind != 'prompt')".into(),
    ];
    if !q.is_empty() {
        sql.push_str(" JOIN events_fts ON events_fts.rowid = e.id");
        wh.push("events_fts MATCH ?".into());
        args.push(Box::new(q.to_string()));
    }
    push_common(&mut wh, &mut args, f, "s.project", "e.ts", "e.session_id");
    wh.push(format!("e.kind IN ({})", vec!["?"; kinds.len()].join(",")));
    for k in kinds {
        args.push(Box::new(k.to_string()));
    }
    sql.push_str(&format!(" WHERE {}", wh.join(" AND ")));
    sql.push_str(match (order, q.is_empty()) {
        ("date_asc", _) => " ORDER BY e.ts ASC",
        ("date_desc", _) | (_, true) => " ORDER BY e.ts DESC",
        _ => " ORDER BY bm25(events_fts) + (strftime('%s','now') * 1000 - e.ts) / 2.592e9",
    });
    sql.push_str(" LIMIT ? OFFSET ?");
    args.push(Box::new(limit));
    args.push(Box::new(offset));
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
        let (id, kind, ts, agent, project, path, t): (
            i64,
            String,
            i64,
            String,
            String,
            String,
            String,
        ) = (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        );
        let body = if t.is_empty() { path } else { squash(&t) };
        Ok((
            ts,
            format!(
                "E{id} [{kind}] {} · {agent} · {} · {}",
                day(ts),
                short(&project),
                text::head(&body, 140)
            ),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn push_common(
    wh: &mut Vec<String>,
    args: &mut Vec<Box<dyn ToSql>>,
    f: &Filters,
    project: &str,
    ts: &str,
    session: &str,
) {
    if let Some(p) = &f.project {
        wh.push(format!("{project} LIKE ?"));
        args.push(Box::new(format!("%{p}%")));
    }
    if let Some(ag) = &f.agent {
        wh.push(format!("{session} LIKE ?"));
        args.push(Box::new(format!("{ag}:%")));
    }
    if let Some(s) = f.start {
        wh.push(format!("{ts} >= ?"));
        args.push(Box::new(s));
    }
    if let Some(e) = f.end {
        wh.push(format!("{ts} < ?"));
        args.push(Box::new(e + 86_400_000));
    }
}

enum Id {
    Memory(i64),
    Event(i64),
}

fn parse_id(v: &Value) -> Option<Id> {
    if let Some(n) = v.as_i64() {
        return Some(Id::Memory(n));
    }
    let s = v.as_str()?.trim().trim_start_matches('#');
    match s.strip_prefix(['E', 'e']) {
        Some(rest) => rest.parse().ok().map(Id::Event),
        None => s.parse().ok().map(Id::Memory),
    }
}

fn timeline(conn: &Connection, a: &Value) -> Result<String> {
    let before = num_arg(a, "depth_before", 3).clamp(0, 50);
    let after = num_arg(a, "depth_after", 3).clamp(0, 50);
    let anchor = match a.get("anchor").and_then(parse_id) {
        Some(id) => id,
        None => {
            let q = str_arg(a, "query").ok_or_else(|| anyhow::anyhow!("pass anchor or query"))?;
            let hit = search_memories(
                conn,
                q,
                None,
                &Filters {
                    project: str_arg(a, "project").map(str::to_string),
                    agent: None,
                    start: None,
                    end: None,
                },
                None,
                &[],
                "relevance",
                1,
                0,
            )?;
            let first = hit
                .0
                .first()
                .ok_or_else(|| anyhow::anyhow!("no match for {q:?}"))?;
            let id: i64 = first
                .1
                .trim_start_matches('#')
                .split(' ')
                .next()
                .unwrap_or("0")
                .parse()?;
            Id::Memory(id)
        }
    };
    match anchor {
        Id::Memory(id) => {
            let (project, at): (Option<String>, i64) = conn
                .query_row(
                    "SELECT project, coalesce(created_at, 0) FROM memories WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("no observation #{id}"))?;
            let row = |r: &rusqlite::Row| -> rusqlite::Result<String> {
                let (i, k, ty, title, t): (i64, String, String, String, i64) =
                    (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
                let mark = if i == id { "→" } else { " " };
                let label = if k == "summary" { "summary".into() } else { ty };
                Ok(format!(
                    "{mark} #{i} [{label}] {} · {}",
                    day(t),
                    text::head(&title, 140)
                ))
            };
            let sel = "SELECT id, kind, coalesce(type, ''), coalesce(title, ''), coalesce(created_at, 0) FROM memories m WHERE project IS ?1";
            // Neighbours leave out scripted sessions' memories; an anchor asked for by id stays.
            let near = format!("{sel} AND {}", crate::scripted::MEMORY_NOT_SCRIPTED);
            let mut prev: Vec<String> = conn
                .prepare(&format!("{near} AND (created_at, id) < (?2, ?3) ORDER BY created_at DESC, id DESC LIMIT ?4"))?
                .query_map(rusqlite::params![project, at, id, before], row)?
                .collect::<rusqlite::Result<_>>()?;
            prev.reverse();
            let this: Vec<String> = conn
                .prepare(&format!("{sel} AND id = ?2"))?
                .query_map(rusqlite::params![project, id], row)?
                .collect::<rusqlite::Result<_>>()?;
            let next: Vec<String> = conn
                .prepare(&format!(
                    "{near} AND (created_at, id) > (?2, ?3) ORDER BY created_at, id LIMIT ?4"
                ))?
                .query_map(rusqlite::params![project, at, id, after], row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok([prev, this, next].concat().join("\n"))
        }
        Id::Event(id) => {
            let session: String = conn
                .query_row("SELECT session_id FROM events WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("no event E{id}"))?;
            let sel = "SELECT id, kind, coalesce(ts, 0), coalesce(path, ''), coalesce(text, '') FROM events
                       WHERE session_id = ?1 AND thread IS NULL";
            let map = |r: &rusqlite::Row| -> rusqlite::Result<(i64, String, i64, String, String)> {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            };
            // Ids interleave across sessions, so walk this session's neighbours by order.
            let mut prev: Vec<_> = conn
                .prepare(&format!("{sel} AND id < ?2 ORDER BY id DESC LIMIT ?3"))?
                .query_map(rusqlite::params![session, id, before], map)?
                .collect::<rusqlite::Result<_>>()?;
            prev.reverse();
            let this: Vec<_> = conn
                .prepare(&format!("{sel} AND id = ?2"))?
                .query_map(rusqlite::params![session, id], map)?
                .collect::<rusqlite::Result<_>>()?;
            let next: Vec<_> = conn
                .prepare(&format!("{sel} AND id > ?2 ORDER BY id LIMIT ?3"))?
                .query_map(rusqlite::params![session, id, after], map)?
                .collect::<rusqlite::Result<_>>()?;
            Ok([prev, this, next]
                .concat()
                .iter()
                .map(|(i, k, ts, p, t)| {
                    let mark = if *i == id { "→" } else { " " };
                    let body = if t.is_empty() { p.clone() } else { squash(t) };
                    format!(
                        "{mark} E{i} [{k}] {} · {}",
                        day(*ts),
                        text::head(&body, 160)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

fn get(conn: &Connection, a: &Value) -> Result<String> {
    let ids = a
        .get("ids")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("ids is required"))?;
    let limit = num_arg(a, "limit", 50).clamp(1, 200) as usize;
    let mut out = Vec::new();
    for v in ids.iter().take(limit) {
        match parse_id(v) {
            Some(Id::Memory(id)) => out.push(memory_detail(conn, id)?),
            Some(Id::Event(id)) => out.push(event_detail(conn, id)?),
            None => out.push(format!("invalid id {v}")),
        }
    }
    Ok(out.join("\n\n---\n\n"))
}

fn recall_file(conn: &Connection, a: &Value) -> Result<String> {
    let path = str_arg(a, "path").unwrap_or_default();
    let cwd = str_arg(a, "cwd")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let limit = num_arg(a, "limit", 5).clamp(1, 50) as usize;
    match crate::files::resolve(path, &cwd) {
        Some(t) => {
            if let Some(s) = str_arg(a, "session") {
                crate::files::claim(conn, s, &t)?;
            }
            crate::files::report(conn, &t, limit)
        }
        None => Ok(format!(
            "{path} is not inside a git repository here, so its project and history are unknown."
        )),
    }
}

fn memory_detail(conn: &Connection, id: i64) -> Result<String> {
    let row = conn
        .query_row(
            "SELECT kind, coalesce(type, ''), coalesce(title, ''), coalesce(subtitle, ''), coalesce(narrative, ''),
                    coalesce(facts, ''), coalesce(concepts, ''), coalesce(files_read, ''), coalesce(files_modified, ''),
                    coalesce(project, ''), coalesce(created_at, 0), coalesce(session_id, '')
             FROM memories WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, String>(11)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, ty, title, subtitle, narrative, facts, concepts, fr, fm, project, at, session)) =
        row
    else {
        return Ok(format!("#{id}: not found"));
    };
    let mut w = format!(
        "#{id} [{}] {title}\n{} · {project} · {session}\n",
        if kind == "summary" { "summary" } else { &ty },
        day(at)
    );
    if !subtitle.is_empty() {
        w.push_str(&format!("{subtitle}\n"));
    }
    if !narrative.is_empty() {
        w.push_str(&format!("\n{narrative}\n"));
    }
    for (label, list) in [
        ("Facts", &facts),
        ("Concepts", &concepts),
        ("Files read", &fr),
        ("Files modified", &fm),
    ] {
        let items: Vec<String> = serde_json::from_str::<Vec<String>>(list).unwrap_or_default();
        if !items.is_empty() {
            w.push_str(&format!("\n{label}:\n"));
            for i in items {
                w.push_str(&format!("- {i}\n"));
            }
        }
    }
    // Whether the files it touched changed since (only files found on this machine).
    let stale = crate::files::staleness_lines(conn, id, 5).unwrap_or_default();
    if !stale.is_empty() {
        w.push_str("\nChanged since this memory (check before relying on it):\n");
        for l in stale {
            w.push_str(&format!("- {l}\n"));
        }
    }
    w.push_str(&evidence(conn, id)?);
    Ok(w)
}

/// event id, hash when linked, kind, text, path, timestamp
type EvidenceRow = (i64, Option<String>, Option<String>, String, String, i64);

/// Where a memory came from: cited transcript events (flagged if they changed since),
/// else the source range, else an explicit "no evidence" for imported history.
fn evidence(conn: &Connection, memory_id: i64) -> Result<String> {
    let mut st = conn.prepare(
        "SELECT v.event_id, v.event_hash, e.kind, coalesce(e.text, ''), coalesce(e.path, ''), coalesce(e.ts, 0)
         FROM memory_evidence v LEFT JOIN events e ON e.id = v.event_id
         WHERE v.memory_id = ?1 ORDER BY v.event_id",
    )?;
    let rows: Vec<EvidenceRow> = st
        .query_map([memory_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if !rows.is_empty() {
        let mut w = String::from("\nEvidence (open with get_observations([\"E<id>\"])):\n");
        for (eid, hash, kind, t, path, ts) in rows {
            let Some(kind) = kind else {
                w.push_str(&format!("- E{eid}: event no longer stored\n"));
                continue;
            };
            let changed = hash.is_some_and(|h| h != text::hash(&format!("{t}{path}")));
            let body = if t.is_empty() { path } else { squash(&t) };
            w.push_str(&format!(
                "- E{eid} [{kind}] {} · {}{}\n",
                day(ts),
                text::head(&body, 140),
                if changed {
                    " (changed since this memory was written)"
                } else {
                    ""
                }
            ));
        }
        return Ok(w);
    }
    let (origin, origin_id): (String, String) = conn.query_row(
        "SELECT origin, origin_id FROM memories WHERE id = ?1",
        [memory_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if origin == "mnem"
        && let Some((session, range)) = origin_id.split_once('@')
        && let Some((from, through)) = range.split('#').next().and_then(|r| r.split_once('-'))
    {
        return Ok(format!(
            "\nSource range: distilled from events E{from}–E{through} of {session} (no per-claim citations; memory predates evidence links).\n"
        ));
    }
    Ok("\nEvidence: none (imported from claude-mem; its sources were not kept).\n".into())
}

fn event_detail(conn: &Connection, id: i64) -> Result<String> {
    let row = conn
        .query_row(
            "SELECT e.kind, coalesce(e.ts, 0), s.agent, coalesce(s.project, ''), e.session_id, e.turn,
                    coalesce(e.tool, ''), coalesce(e.path, ''), coalesce(e.label, ''), coalesce(e.text, '')
             FROM events e JOIN sessions s ON s.id = e.session_id WHERE e.id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, ts, agent, project, session, turn, tool, path, label, t)) = row else {
        return Ok(format!("E{id}: not found"));
    };
    let mut w = format!(
        "E{id} [{kind}] {} · {agent} · {project} · {session} turn {}\n",
        day(ts),
        turn.unwrap_or(0)
    );
    for (k, v) in [("tool", &tool), ("path", &path), ("label", &label)] {
        if !v.is_empty() {
            w.push_str(&format!("{k}: {v}\n"));
        }
    }
    w.push_str(&format!("\n{t}\n"));
    // A prompt is most useful with the answer it got.
    if kind == "prompt"
        && let Some(turn) = turn
        && let Some(answer) = context::final_answer(conn, &session, turn)?
    {
        w.push_str(&format!("\nFinal answer:\n{answer}\n"));
    }
    Ok(w)
}

fn short(project: &str) -> String {
    // A teammate's project merged in under a prefix keeps the prefix: "ana/acme/shop".
    if let Some((prefix, rest)) = project.split_once('/')
        && !prefix.contains('.')
        && rest
            .split('/')
            .next()
            .is_some_and(|host| host.contains('.'))
    {
        return format!("{prefix}/{}", short(rest));
    }
    project
        .rsplit('/')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/")
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod short_tests {
    #[test]
    fn merged_projects_keep_their_prefix() {
        assert_eq!(super::short("github.com/acme/shop"), "acme/shop");
        assert_eq!(super::short("ana/github.com/acme/shop"), "ana/acme/shop");
        assert_eq!(super::short("/home/me/code"), "me/code");
        assert_eq!(super::short("automation-research"), "automation-research");
    }
}

#[cfg(test)]
mod tests {
    use super::{day, handle};

    fn conn() -> rusqlite::Connection {
        let p = std::env::temp_dir().join(format!("mnem-mcp-{}.db", std::process::id()));
        crate::db::open(&p).unwrap()
    }

    #[test]
    fn protocol_contract() {
        let c = conn();
        let r = handle(&c, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-99-99"}}"#).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        let r = handle(&c, r#"{"id":{"x":1},"method":"ping"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32600);
        assert!(
            handle(
                &c,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .is_none()
        );
        let r = handle(
            &c,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope"}}"#,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32602);
        let r = handle(&c, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":{"a":1}}}}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        let r = handle(&c, r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search","arguments":{"query":"x","limit":-1}}}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }

    #[test]
    fn plain_language_search_finds_partial_matches() {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        for (id, title) in [
            (1, "Webhook retries fail while port 3003 is taken"),
            (2, "Deploy notes for the staging cluster"),
        ] {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, origin, origin_id, created_at)
                 VALUES (?1, 'p', 'observation', 'bugfix', ?2, 'mnem', ?1, 0)",
                rusqlite::params![id, title],
            )
            .unwrap();
        }
        let ask = |q: &str| {
            super::call(
                &c,
                "search",
                &serde_json::json!({ "query": q, "type": "observations" }),
            )
            .unwrap()
        };
        // Not every word of a question appears in the memory; it must still be found.
        let r = ask("why do my webhook retries keep failing");
        assert!(r.starts_with("#1 "), "{r}");
        assert!(!r.contains("#2 "), "{r}");
        assert!(ask("webhook 3003").starts_with("#1 "));
    }

    fn memory_db() -> rusqlite::Connection {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        for id in 1..=6 {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, origin, origin_id, created_at)
                 VALUES (?1, 'p', 'observation', ?2, ?3, 'mnem', ?1, ?1)",
                rusqlite::params![
                    id,
                    if id == 6 { "sensitive" } else { "bugfix" },
                    format!("cache eviction bug number {id}")
                ],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, project) VALUES ('claude:s', 'claude', 's', 'p')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('claude:s', 'k', 9, 'prompt', 'cache eviction again')",
            [],
        )
        .unwrap();
        c
    }

    fn ids(r: &str) -> Vec<String> {
        r.lines()
            .filter(|l| l.starts_with('#') || l.starts_with('E'))
            .map(|l| l.split(' ').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn search_pages_are_stable_and_global() {
        let c = memory_db();
        let ask = |v: serde_json::Value| ids(&super::call(&c, "search", &v).unwrap());
        let q = "cache eviction";
        let whole = ask(serde_json::json!({ "query": q, "limit": 4, "type": "observations" }));
        let mut paged = ask(serde_json::json!({ "query": q, "limit": 2, "type": "observations" }));
        paged.extend(ask(
            serde_json::json!({ "query": q, "limit": 2, "offset": 2, "type": "observations" }),
        ));
        assert_eq!(whole, paged);
        // Without a type, one page spans memories and events and holds `limit` rows.
        assert_eq!(ask(serde_json::json!({ "query": q, "limit": 1 })).len(), 1);
        let all = ask(serde_json::json!({ "query": q, "limit": 20 }));
        assert_eq!(all.len(), 6, "{all:?}");
        assert_eq!(all.last().unwrap(), "E1");
        // Sensitive memories only when asked for by type.
        assert!(!all.contains(&"#6".to_string()));
        assert_eq!(
            ask(serde_json::json!({ "query": q, "type": "observations", "obs_type": "sensitive" })),
            vec!["#6"]
        );
        // Date order interleaves both tables by time.
        let dated = ask(serde_json::json!({ "query": q, "orderBy": "date_desc", "limit": 2 }));
        assert_eq!(dated, vec!["E1", "#5"]);
    }

    #[test]
    fn formats_days() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_790_507_604_101), "2026-09-27");
    }
}
