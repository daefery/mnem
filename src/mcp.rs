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

const INSTRUCTIONS: &str = "mnem is long-term memory built from every Claude Code, Codex and pi session. \
Workflow: 1) search(query) returns a compact index with ids; 2) timeline(anchor) shows what happened around one; \
3) get_observations(ids) fetches full details only for the ids you need. Numeric ids are distilled observations \
and summaries; ids like \"E123\" are raw transcript events (prompts, answers, commands, errors, edits).";

pub fn serve(conn: &Connection) -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}),
                )?;
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        }; // notification
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mnem", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match call(conn, name, &args) {
                    Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                    Err(e) => {
                        json!({ "content": [{ "type": "text", "text": format!("error: {e:#}") }], "isError": true })
                    }
                })
            }
            _ => Err(json!({ "code": -32601, "message": format!("method not found: {method}") })),
        };
        let resp = match reply {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        write_msg(&mut out, &resp)?;
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
            "description": "Step 1: Search memory. Returns an index with ids. Params: query, limit, project, platformSource, type, obs_type, dateStart, dateEnd, offset, orderBy",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "Search query (empty: most recent)" },
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
            "description": "Step 2: Context around a result. Params: anchor (observation id, or \"E<id>\" event) OR query, depth_before, depth_after, project",
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
            "description": "Step 3: Full details for ids from search/timeline. Params: ids (required; numbers or \"E<id>\"), limit",
            "inputSchema": { "type": "object", "required": ["ids"], "properties": {
                "ids": { "type": "array", "items": { "type": ["number", "string"] }, "description": "Ids to fetch" },
                "limit": { "type": "number", "description": "Max items" }
            }}
        },
        {
            "name": "session_start_context",
            "description": "The context mnem injects at session start for a project: recent sessions across agents, last summary, observations.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": "Project id (default: this server's working directory)" },
                "cwd": { "type": "string", "description": "Resolve the project from this directory" }
            }}
        }
    ])
}

pub fn call(conn: &Connection, name: &str, a: &Value) -> Result<String> {
    match name {
        "search" => search(conn, a),
        "timeline" => timeline(conn, a),
        "get_observations" => get(conn, a),
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

fn search(conn: &Connection, a: &Value) -> Result<String> {
    let q = str_arg(a, "query").map(fts_query).unwrap_or_default();
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
    let mut out = Vec::new();
    let want_mem = matches!(
        kind.as_deref(),
        None | Some("observations") | Some("sessions")
    );
    let want_ev = matches!(kind.as_deref(), None | Some("prompts") | Some("events"));
    if want_mem {
        let mem_kind = match kind.as_deref() {
            Some("sessions") => Some("summary"),
            Some("observations") => Some("observation"),
            _ => None,
        };
        out.extend(search_memories(
            conn, &q, &f, mem_kind, &obs_types, order, limit, offset,
        )?);
    }
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
        out.extend(search_events(conn, &q, &f, kinds, order, limit, offset)?);
    }
    if out.is_empty() {
        return Ok("No results.".into());
    }
    Ok(format!(
        "{}\n\nNext: timeline(anchor) or get_observations([ids]).",
        out.join("\n")
    ))
}

#[allow(clippy::too_many_arguments)]
fn search_memories(
    conn: &Connection,
    q: &str,
    f: &Filters,
    kind: Option<&str>,
    types: &[String],
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<String>> {
    let mut sql = String::from(
        "SELECT m.id, m.kind, coalesce(m.type, ''), coalesce(m.title, ''), coalesce(m.created_at, 0), coalesce(m.project, '')
         FROM memories m",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut wh: Vec<String> = Vec::new();
    if !q.is_empty() {
        sql.push_str(" JOIN memories_fts ON memories_fts.rowid = m.id");
        wh.push("memories_fts MATCH ?".into());
        args.push(Box::new(q.to_string()));
    }
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
    if !types.is_empty() {
        wh.push(format!("m.type IN ({})", vec!["?"; types.len()].join(",")));
        for t in types {
            args.push(Box::new(t.clone()));
        }
    }
    if !wh.is_empty() {
        sql.push_str(&format!(" WHERE {}", wh.join(" AND ")));
    }
    sql.push_str(match (order, q.is_empty()) {
        ("date_asc", _) => " ORDER BY m.created_at ASC",
        ("date_desc", _) | (_, true) => " ORDER BY m.created_at DESC",
        _ => {
            " ORDER BY bm25(memories_fts) + (strftime('%s','now') * 1000 - m.created_at) / 2.592e9"
        }
    });
    sql.push_str(" LIMIT ? OFFSET ?");
    args.push(Box::new(limit));
    args.push(Box::new(offset));
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
        let (id, kind, ty, title, at, project): (i64, String, String, String, i64, String) = (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        );
        let label = if kind == "summary" {
            "summary".to_string()
        } else {
            ty
        };
        Ok(format!(
            "#{id} [{label}] {} · {} · {}",
            day(at),
            short(&project),
            text::head(&title, 140)
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn search_events(
    conn: &Connection,
    q: &str,
    f: &Filters,
    kinds: &[&str],
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<String>> {
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
        Ok(format!(
            "E{id} [{kind}] {} · {agent} · {} · {}",
            day(ts),
            short(&project),
            text::head(&body, 140)
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
                &fts_query(q),
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
                .first()
                .ok_or_else(|| anyhow::anyhow!("no match for {q:?}"))?;
            let id: i64 = first
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
            let sel = "SELECT id, kind, coalesce(type, ''), coalesce(title, ''), coalesce(created_at, 0) FROM memories WHERE project IS ?1";
            let mut prev: Vec<String> = conn
                .prepare(&format!("{sel} AND (created_at, id) < (?2, ?3) ORDER BY created_at DESC, id DESC LIMIT ?4"))?
                .query_map(rusqlite::params![project, at, id, before], row)?
                .collect::<rusqlite::Result<_>>()?;
            prev.reverse();
            let this: Vec<String> = conn
                .prepare(&format!("{sel} AND id = ?2"))?
                .query_map(rusqlite::params![project, id], row)?
                .collect::<rusqlite::Result<_>>()?;
            let next: Vec<String> = conn
                .prepare(&format!(
                    "{sel} AND (created_at, id) > (?2, ?3) ORDER BY created_at, id LIMIT ?4"
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
            let mut st = conn.prepare(
                "SELECT id, kind, coalesce(ts, 0), coalesce(path, ''), coalesce(text, '') FROM events
                 WHERE session_id = ?1 AND thread IS NULL AND id BETWEEN ?2 AND ?3 ORDER BY id",
            )?;
            // Ids interleave across sessions, so over-fetch the window and trim.
            let rows: Vec<(i64, String, i64, String, String)> = st
                .query_map(rusqlite::params![session, id - 5000, id + 5000], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let pos = rows.iter().position(|r| r.0 == id).unwrap_or(0);
            let lo = pos.saturating_sub(before as usize);
            let hi = (pos + after as usize + 1).min(rows.len());
            Ok(rows[lo..hi]
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
    Ok(w)
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
mod tests {
    use super::day;

    #[test]
    fn formats_days() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_790_507_604_101), "2026-09-27");
    }
}
