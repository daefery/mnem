//! The record API, v1: a stable, read-only HTTP interface to everything mnem captured, so
//! other tools build on one complete record instead of each parsing agent transcripts.
//! Served by `mnem watch` (and `mnem ui`) on 127.0.0.1 next to the viewer, under `/v1/`.
//!
//! Contract (docs/api.md): field names and meanings in v1 only ever gain fields, never
//! lose or change them; anything else is /v2. Every request carries
//! `Authorization: Bearer <token>`, the token in `~/.mnem/api-token` (owner-only). A web
//! page on another site cannot send that header without a CORS preflight, which this
//! server never answers, and the viewer's Host check stops DNS rebinding, so a browser
//! cannot read the record; a local tool reads the token file as it could read the
//! database itself. Lists page by id: pass back `next` as `after` to continue, which also
//! lets a tool sync incrementally. Personal details (type "sensitive") are left out
//! unless `include=sensitive` asks for them, as in MCP search.

use crate::db;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Bumped only for a breaking change, which also moves the routes to /v2.
pub const VERSION: u32 = 1;
const MAX_LIMIT: i64 = 1000;
const DEFAULT_LIMIT: i64 = 100;

/// An API answer: an HTTP status and a JSON body.
pub struct Reply {
    pub status: &'static str,
    pub body: Value,
}

fn ok(body: Value) -> Reply {
    Reply {
        status: "200 OK",
        body,
    }
}

fn error(status: &'static str, message: impl std::fmt::Display) -> Reply {
    Reply {
        status,
        body: json!({ "error": message.to_string() }),
    }
}

/// Where the token lives.
pub fn token_path() -> PathBuf {
    db::data_dir().join("api-token")
}

/// The API token, created (owner-only, 256 random bits) the first time it is needed.
pub fn token() -> Result<String> {
    let path = token_path();
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 32 {
            return Ok(t);
        }
    }
    let mut bytes = [0u8; 32];
    random(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(path.parent().context("data dir")?)?;
    // Written to a private temporary file, then moved into place: never readable by others.
    let tmp = path.with_extension("tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        use std::io::Write;
        o.open(&tmp)?.write_all(token.as_bytes())?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(token)
}

fn random(buf: &mut [u8]) -> Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .context("read /dev/urandom")
}

/// Compare without stopping at the first difference, so timing says nothing about the token.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Answer a GET under /v1 (`path` without the /v1 prefix), after checking the token.
pub fn handle(
    path: &str,
    q: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    db_path: &Path,
) -> Reply {
    let Ok(want) = token() else {
        return error("500 Internal Server Error", "no API token");
    };
    let given = headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    if !given.is_some_and(|g| same(g, &want)) {
        return error(
            "401 Unauthorized",
            format!(
                "send Authorization: Bearer <token>, the token in {}",
                token_path().display()
            ),
        );
    }
    let conn = match crate::ui::open(db_path) {
        Ok(c) => c,
        Err(e) => return error("500 Internal Server Error", format!("{e:#}")),
    };
    let result = match path.trim_end_matches('/') {
        "" => meta(&conn),
        "/sessions" => sessions(&conn, q),
        "/events" => events(&conn, q),
        "/memories" => memories(&conn, q),
        "/search" => search(&conn, q),
        p => match p.strip_prefix("/sessions/") {
            Some(id) => one_session(&conn, id),
            None => match p.strip_prefix("/memories/").map(str::parse::<i64>) {
                Some(Ok(id)) => one_memory(&conn, id),
                Some(Err(_)) => Ok(error("400 Bad Request", "memory id must be a number")),
                None => Ok(error("404 Not Found", "no such endpoint; see GET /v1")),
            },
        },
    };
    result.unwrap_or_else(|e| error("500 Internal Server Error", format!("{e:#}")))
}

fn limit(q: &HashMap<String, String>) -> std::result::Result<i64, Reply> {
    match q.get("limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| error("400 Bad Request", format!("limit must be 1 to {MAX_LIMIT}"))),
    }
}

/// A number parameter, or a 400 naming it.
fn number(q: &HashMap<String, String>, name: &str) -> std::result::Result<Option<i64>, Reply> {
    match q.get(name) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| error("400 Bad Request", format!("{name} must be a number"))),
    }
}

/// A paged list: the query, its filters, and how rows become items.
struct List<'a> {
    /// `SELECT … FROM …` without WHERE, ORDER or LIMIT.
    sql: &'a str,
    wh: Vec<String>,
    args: Vec<Box<dyn ToSql>>,
    /// The column pages are ordered and cut by, and the item field that holds it.
    id_col: &'a str,
    cursor_field: &'a str,
    row: &'a dyn Fn(&rusqlite::Row) -> rusqlite::Result<Value>,
}

/// Run a paged list: rows after `after` by id, ascending, at most `limit`; `next` is the
/// cursor for the following page, or null at the end.
fn page(conn: &Connection, q: &HashMap<String, String>, l: List) -> Result<Reply> {
    let List {
        sql,
        mut wh,
        mut args,
        id_col,
        cursor_field,
        row,
    } = l;
    let limit = match limit(q) {
        Ok(n) => n,
        Err(r) => return Ok(r),
    };
    match number(q, "after") {
        Ok(Some(after)) => {
            wh.push(format!("{id_col} > ?"));
            args.push(Box::new(after));
        }
        Ok(None) => {}
        Err(r) => return Ok(r),
    }
    let filter = if wh.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wh.join(" AND "))
    };
    args.push(Box::new(limit + 1));
    let mut st = conn.prepare(&format!("{sql}{filter} ORDER BY {id_col} LIMIT ?"))?;
    let mut items: Vec<Value> = st
        .query_map(params_from_iter(args.iter().map(|b| b.as_ref())), row)?
        .collect::<rusqlite::Result<_>>()?;
    let more = items.len() as i64 > limit;
    items.truncate(limit as usize);
    let next = if more {
        items.last().and_then(|v| v[cursor_field].as_i64())
    } else {
        None
    };
    Ok(ok(json!({ "items": items, "next": next })))
}

fn meta(conn: &Connection) -> Result<Reply> {
    let count = |t: &str| -> Result<i64> {
        Ok(conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?)
    };
    Ok(ok(json!({
        "api": VERSION,
        "mnem": env!("CARGO_PKG_VERSION"),
        "embedding_model": crate::embed::model_name(),
        "counts": { "sessions": count("sessions")?, "events": count("events")?, "memories": count("memories")? },
        "endpoints": ["/v1/sessions", "/v1/sessions/{id}", "/v1/events", "/v1/memories", "/v1/memories/{id}", "/v1/search"],
        "docs": "https://github.com/daefery/mnem/blob/main/docs/api.md",
    })))
}

const SESSION_COLS: &str = "s.id, s.agent, s.native_id, s.project, s.cwd, s.git_branch, s.title, s.started_at, s.last_event_at,
     EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = s.id)";

fn session_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "agent": r.get::<_, String>(1)?,
        "native_id": r.get::<_, String>(2)?,
        "project": r.get::<_, Option<String>>(3)?,
        "cwd": r.get::<_, Option<String>>(4)?,
        "git_branch": r.get::<_, Option<String>>(5)?,
        "title": r.get::<_, Option<String>>(6)?,
        "started_at": r.get::<_, Option<i64>>(7)?,
        "last_event_at": r.get::<_, Option<i64>>(8)?,
        "scripted": r.get::<_, bool>(9)?,
    }))
}

/// Sessions page by their rowid (their ids are text): `next` is a rowid to pass as
/// `after`, and each item carries it as `cursor`.
fn sessions(conn: &Connection, q: &HashMap<String, String>) -> Result<Reply> {
    let (mut wh, mut args): (Vec<String>, Vec<Box<dyn ToSql>>) = (vec![], vec![]);
    if let Some(p) = q.get("project") {
        wh.push("s.project = ?".into());
        args.push(Box::new(p.clone()));
    }
    if let Some(a) = q.get("agent") {
        wh.push("s.agent = ?".into());
        args.push(Box::new(a.clone()));
    }
    match number(q, "since") {
        Ok(Some(t)) => {
            wh.push("s.last_event_at >= ?".into());
            args.push(Box::new(t));
        }
        Ok(None) => {}
        Err(r) => return Ok(r),
    }
    page(
        conn,
        q,
        List {
            sql: &format!("SELECT {SESSION_COLS}, s.rowid FROM sessions s"),
            wh,
            args,
            id_col: "s.rowid",
            cursor_field: "cursor",
            row: &|r| {
                let mut v = session_json(r)?;
                v["cursor"] = json!(r.get::<_, i64>(10)?);
                Ok(v)
            },
        },
    )
}

fn one_session(conn: &Connection, id: &str) -> Result<Reply> {
    let id = percent_decode(id);
    let s = conn
        .query_row(
            &format!("SELECT {SESSION_COLS} FROM sessions s WHERE s.id = ?1"),
            [&id],
            session_json,
        )
        .optional()?;
    let Some(mut s) = s else {
        return Ok(error("404 Not Found", "no such session"));
    };
    let (events, memories): (i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM events WHERE session_id = ?1), (SELECT count(*) FROM memories WHERE session_id = ?1)",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    s["events"] = json!(events);
    s["memories"] = json!(memories);
    Ok(ok(s))
}

fn events(conn: &Connection, q: &HashMap<String, String>) -> Result<Reply> {
    let (mut wh, mut args): (Vec<String>, Vec<Box<dyn ToSql>>) = (vec![], vec![]);
    if let Some(s) = q.get("session") {
        wh.push("e.session_id = ?".into());
        args.push(Box::new(s.clone()));
    }
    if let Some(k) = q.get("kind") {
        wh.push("e.kind = ?".into());
        args.push(Box::new(k.clone()));
    }
    if let Some(p) = q.get("project") {
        wh.push("e.session_id IN (SELECT id FROM sessions WHERE project = ?)".into());
        args.push(Box::new(p.clone()));
    }
    page(
        conn,
        q,
        List {
            sql: "SELECT e.id, e.session_id, e.ts, e.turn, e.kind, e.tool, e.path, e.text, e.is_error, e.label, e.thread FROM events e",
            wh,
            args,
            id_col: "e.id",
            cursor_field: "id",
            row: &|r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "session_id": r.get::<_, String>(1)?,
                    "ts": r.get::<_, Option<i64>>(2)?,
                    "turn": r.get::<_, Option<i64>>(3)?,
                    "kind": r.get::<_, String>(4)?,
                    "tool": r.get::<_, Option<String>>(5)?,
                    "path": r.get::<_, Option<String>>(6)?,
                    "text": r.get::<_, Option<String>>(7)?,
                    "is_error": r.get::<_, bool>(8)?,
                    "label": r.get::<_, Option<String>>(9)?,
                    "subagent": r.get::<_, Option<String>>(10)?,
                }))
            },
        },
    )
}

const MEMORY_COLS: &str = "m.id, m.session_id, m.project, m.kind, m.type, m.title, m.subtitle, m.narrative, m.facts, m.concepts,
     m.files_read, m.files_modified, m.origin, m.model, m.created_at";

fn memory_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let list = |i: usize| -> rusqlite::Result<Value> {
        Ok(r.get::<_, Option<String>>(i)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!([])))
    };
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "session_id": r.get::<_, Option<String>>(1)?,
        "project": r.get::<_, Option<String>>(2)?,
        "kind": r.get::<_, String>(3)?,
        "type": r.get::<_, Option<String>>(4)?,
        "title": r.get::<_, Option<String>>(5)?,
        "subtitle": r.get::<_, Option<String>>(6)?,
        "narrative": r.get::<_, Option<String>>(7)?,
        "facts": list(8)?,
        "concepts": list(9)?,
        "files_read": list(10)?,
        "files_modified": list(11)?,
        "origin": r.get::<_, String>(12)?,
        "model": r.get::<_, Option<String>>(13)?,
        "created_at": r.get::<_, Option<i64>>(14)?,
    }))
}

fn include_sensitive(q: &HashMap<String, String>) -> bool {
    q.get("include")
        .is_some_and(|v| v.split(',').any(|x| x.trim() == "sensitive"))
}

fn memories(conn: &Connection, q: &HashMap<String, String>) -> Result<Reply> {
    let (mut wh, mut args): (Vec<String>, Vec<Box<dyn ToSql>>) = (vec![], vec![]);
    if !include_sensitive(q) {
        wh.push("coalesce(m.type, '') != 'sensitive'".into());
    }
    for (param, col) in [
        ("project", "m.project"),
        ("kind", "m.kind"),
        ("type", "m.type"),
        ("session", "m.session_id"),
    ] {
        if let Some(v) = q.get(param) {
            wh.push(format!("{col} = ?"));
            args.push(Box::new(v.clone()));
        }
    }
    match number(q, "since") {
        Ok(Some(t)) => {
            wh.push("m.created_at >= ?".into());
            args.push(Box::new(t));
        }
        Ok(None) => {}
        Err(r) => return Ok(r),
    }
    page(
        conn,
        q,
        List {
            sql: &format!("SELECT {MEMORY_COLS} FROM memories m"),
            wh,
            args,
            id_col: "m.id",
            cursor_field: "id",
            row: &memory_json,
        },
    )
}

/// One memory with its evidence (the transcript events it cites) and, per file it
/// modified, whether its session's edits are still in the file now.
fn one_memory(conn: &Connection, id: i64) -> Result<Reply> {
    let m = conn
        .query_row(
            &format!("SELECT {MEMORY_COLS} FROM memories m WHERE m.id = ?1"),
            [id],
            memory_json,
        )
        .optional()?;
    let Some(mut m) = m else {
        return Ok(error("404 Not Found", "no such memory"));
    };
    let mut st = conn.prepare(
        "SELECT v.event_id, v.relation, e.kind, e.ts, substr(coalesce(e.text, ''), 1, 300)
           FROM memory_evidence v LEFT JOIN events e ON e.id = v.event_id
          WHERE v.memory_id = ?1 ORDER BY v.event_id",
    )?;
    let evidence: Vec<Value> = st
        .query_map([id], |r| {
            Ok(json!({
                "event_id": r.get::<_, i64>(0)?,
                "relation": r.get::<_, String>(1)?,
                "kind": r.get::<_, Option<String>>(2)?,
                "ts": r.get::<_, Option<i64>>(3)?,
                "excerpt": r.get::<_, Option<String>>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    m["evidence"] = json!(evidence);
    m["files"] = json!(file_survival(conn, id)?);
    Ok(ok(m))
}

/// Per file the memory modified: whether its own edited lines are still there
/// (`kept` of `of` lines), or null when that cannot be told on this machine.
fn file_survival(conn: &Connection, id: i64) -> Result<Vec<Value>> {
    let project: Option<String> = conn
        .query_row("SELECT project FROM memories WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    let cwd: Option<String> = match &project {
        Some(p) => conn
            .query_row(
                "SELECT cwd FROM sessions WHERE (project = ?1 OR project LIKE ?1 || '#%') AND cwd IS NOT NULL
                 ORDER BY last_event_at DESC LIMIT 1",
                [p],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    let root = cwd.and_then(|c| crate::files::repo_root(Path::new(&c)));
    let mut st = conn.prepare(
        "SELECT DISTINCT path FROM memory_files WHERE memory_id = ?1 AND modified = 1 ORDER BY path LIMIT 20",
    )?;
    let paths: Vec<String> = st
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for p in paths {
        // Recorded paths are absolute or repo-relative; placed the way get_observations does.
        let rel = root.as_deref().and_then(|r| crate::files::local_rel(r, &p));
        let kept = match (&root, &rel) {
            (Some(r), Some(rel)) => crate::files::edits_kept(conn, id, r, rel).ok().flatten(),
            _ => None,
        };
        out.push(json!({
            "path": p,
            "edits_kept": kept.map(|k| json!({ "kept": k.kept, "of": k.of, "intact": k.intact() })),
        }));
    }
    Ok(out)
}

/// Memories ranked for a query: every-word matches, then any meaningful word, fused
/// with the nearest by meaning when the embedding model is loaded (as MCP search).
fn search(conn: &Connection, q: &HashMap<String, String>) -> Result<Reply> {
    let Some(text) = q.get("q").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(error("400 Bad Request", "q is required"));
    };
    let limit = match limit(q) {
        Ok(n) => n.min(200),
        Err(r) => return Ok(r),
    };
    let sensitive = include_sensitive(q);
    let project = q.get("project").cloned();
    let filter = format!(
        "{} AND ({} OR m.project = ?)",
        if sensitive {
            "1 = 1"
        } else {
            "coalesce(m.type, '') != 'sensitive'"
        },
        if project.is_none() { "1 = 1" } else { "0 = 1" }
    );
    let args = || -> Vec<Box<dyn ToSql>> { vec![Box::new(project.clone().unwrap_or_default())] };
    let vq = crate::embed::shared().map(|e| e.query(text));
    let ranked = crate::search::rank_memories(conn, text, vq.as_ref(), &filter, &args)?;
    let mut items = Vec::new();
    let mut st = conn.prepare_cached(&format!(
        "SELECT {MEMORY_COLS} FROM memories m WHERE m.id = ?1"
    ))?;
    for r in ranked.into_iter().take(limit as usize) {
        let mut m = st.query_row(params![r.id], memory_json)?;
        m["match"] = json!(r.how());
        items.push(m);
    }
    Ok(ok(json!({ "items": items, "semantic": vq.is_some() })))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
