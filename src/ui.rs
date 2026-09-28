//! `mnem ui`: a local web viewer for the memory database.
//!
//! A deliberately small HTTP/1.1 server (GET only, one thread per connection, no
//! framework). It binds to 127.0.0.1 and rejects requests whose Host header is not
//! local, so other sites in the browser cannot read memory via DNS rebinding.

use crate::context;
use crate::db;
use crate::search::fts_query;
use anyhow::Result;
use rusqlite::{Connection, ToSql, params_from_iter};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const INDEX: &str = include_str!("../ui/index.html");
/// Largest request head (request line + headers) the server reads.
const MAX_HEAD: usize = 16 * 1024;
/// Most connections handled at once.
const MAX_CONNECTIONS: usize = 32;
/// Longest text /api/embed embeds (longer queries are cut, not rejected).
const MAX_EMBED_CHARS: usize = 2000;
const APP: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");
const LOGO: &str = include_str!("../ui/logo.svg");
const FONT: &[u8] = include_bytes!("../ui/fonts/monaspace-radon-var.woff2");
const ICONS: &[(&str, &str)] = &[
    (
        "icon-thick-investigated.svg",
        include_str!("../ui/icons/icon-thick-investigated.svg"),
    ),
    (
        "icon-thick-learned.svg",
        include_str!("../ui/icons/icon-thick-learned.svg"),
    ),
    (
        "icon-thick-completed.svg",
        include_str!("../ui/icons/icon-thick-completed.svg"),
    ),
    (
        "icon-thick-next-steps.svg",
        include_str!("../ui/icons/icon-thick-next-steps.svg"),
    ),
];

/// Serve until the process ends. `bound` runs once the port is listening.
pub fn serve(db_path: PathBuf, port: u16, bound: impl FnOnce()) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    bound();
    println!("mnem ui: http://127.0.0.1:{port}/  (Ctrl-C to stop)");
    static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // A bounded number of handlers: a local flood cannot exhaust the watch process.
        if ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_CONNECTIONS {
            ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let db_path = db_path.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &db_path, port) {
                eprintln!("mnem ui: {e:#}");
            }
            ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    Ok(())
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    cache: bool,
}

impl Response {
    fn text(
        status: &'static str,
        content_type: &'static str,
        body: impl Into<Vec<u8>>,
    ) -> Response {
        Response {
            status,
            content_type,
            body: body.into(),
            cache: false,
        }
    }
    fn json(v: &Value) -> Response {
        Response::text("200 OK", "application/json; charset=utf-8", v.to_string())
    }
}

fn handle(mut stream: TcpStream, db_path: &Path, port: u16) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    // The whole request head is capped; nothing larger is ever buffered.
    let mut reader = BufReader::new(std::io::Read::take(stream.try_clone()?, MAX_HEAD as u64));
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if !request_line.ends_with('\n') {
        let resp = Response::text("414 URI Too Long", "text/plain", "request too large\n");
        let head = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            resp.status,
            resp.body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&resp.body)?;
        return Ok(());
    }
    let mut host = String::new();
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        header_bytes += n;
        if n == 0 || line == "\r\n" || line == "\n" || header_bytes > 16 * 1024 {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("host")
        {
            host = v.trim().to_string();
        }
    }
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let local = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    let resp = if !local.contains(&host) {
        Response::text(
            "403 Forbidden",
            "text/plain",
            "mnem ui only answers local requests\n",
        )
    } else if method != "GET" {
        Response::text("405 Method Not Allowed", "text/plain", "GET only\n")
    } else {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        route(path, &parse_query(query), db_path).unwrap_or_else(|e| {
            Response::text(
                "500 Internal Server Error",
                "text/plain",
                format!("{e:#}\n"),
            )
        })
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        resp.status,
        resp.content_type,
        resp.body.len(),
        if resp.cache {
            "max-age=86400"
        } else {
            "no-cache"
        }
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    Ok(())
}

fn route(path: &str, q: &HashMap<String, String>, db_path: &Path) -> Result<Response> {
    Ok(match path {
        "/" | "/index.html" => Response::text("200 OK", "text/html; charset=utf-8", INDEX),
        "/app.js" => Response::text("200 OK", "text/javascript; charset=utf-8", APP),
        "/style.css" => Response::text("200 OK", "text/css; charset=utf-8", STYLE),
        "/logo.svg" => Response::text("200 OK", "image/svg+xml", LOGO),
        "/fonts/monaspace-radon-var.woff2" => Response {
            status: "200 OK",
            content_type: "font/woff2",
            body: FONT.to_vec(),
            cache: true,
        },
        p if p.starts_with("/icons/") => match ICONS.iter().find(|(n, _)| p.ends_with(n)) {
            Some((_, svg)) => Response {
                status: "200 OK",
                content_type: "image/svg+xml",
                body: svg.as_bytes().to_vec(),
                cache: true,
            },
            None => not_found(),
        },
        "/api/feed" => Response::json(&feed(&open(db_path)?, q)?),
        "/api/projects" => Response::json(&projects(&open(db_path)?)?),
        "/api/stats" => Response::json(&stats(&open(db_path)?)?),
        "/api/embed" => {
            let text = crate::text::head(
                q.get("q").map(String::as_str).unwrap_or_default(),
                MAX_EMBED_CHARS,
            );
            match crate::embed::shared() {
                Some(e) => {
                    let query = e.query(&text);
                    Response::json(&json!({ "model": query.model, "vector": query.vec }))
                }
                None => Response::text(
                    "503 Service Unavailable",
                    "text/plain",
                    "no embedding model loaded\n",
                ),
            }
        }
        "/api/context" => {
            let conn = open(db_path)?;
            let project = match q.get("project").filter(|p| !p.is_empty()) {
                Some(p) => p.clone(),
                None => conn.query_row(
                    "SELECT project FROM sessions WHERE project IS NOT NULL ORDER BY last_event_at DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )?,
            };
            let text = context::build(
                &conn,
                &context::Options {
                    project: &project,
                    current: None,
                    budget_chars: 8000,
                    sessions: 5,
                    turns: 3,
                    observations: 30,
                },
            )?;
            Response::text("200 OK", "text/plain; charset=utf-8", text)
        }
        _ => not_found(),
    })
}

fn not_found() -> Response {
    Response::text("404 Not Found", "text/plain", "not found\n")
}

fn open(db_path: &Path) -> Result<Connection> {
    db::open_with(db_path, Duration::from_secs(2))
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_list(s: Option<String>) -> Value {
    s.and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_array)
        .unwrap_or(json!([]))
}

fn agent_of(session: Option<&str>) -> String {
    session
        .and_then(|s| s.split_once(':'))
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| "claude".into())
}

/// Summary fields: structured `data` when present, else the "Label: text" narrative.
fn summary_fields(
    title: Option<String>,
    narrative: Option<String>,
    data: Option<String>,
) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    let data: Value = data
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or(Value::Null);
    let keys = [
        ("request", "Request"),
        ("investigated", "Investigated"),
        ("learned", "Learned"),
        ("completed", "Completed"),
        ("next_steps", "Next steps"),
    ];
    for (k, label) in keys {
        let v = data
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                narrative
                    .as_deref()?
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{label}: ")).map(str::to_string))
            });
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            m.insert(k.into(), json!(v));
        }
    }
    if !m.contains_key("request")
        && let Some(t) = title.filter(|t| !t.is_empty())
    {
        m.insert("request".into(), json!(t));
    }
    m
}

const MEMORY_COLS: &str = "m.id, m.kind, m.type, m.title, m.subtitle, m.narrative, m.facts, m.concepts, m.files_read,
                m.files_modified, m.data, m.project, m.session_id, coalesce(m.created_at, 0), m.origin";

/// A memory row (selected with MEMORY_COLS) as a feed item.
fn memory_item(r: &rusqlite::Row) -> Result<(i64, Value)> {
    let id: i64 = r.get(0)?;
    let kind: String = r.get(1)?;
    let session: Option<String> = r.get(12)?;
    let at: i64 = r.get(13)?;
    let base = json!({
        "id": id,
        "project": r.get::<_, Option<String>>(11)?,
        "platform_source": agent_of(session.as_deref()),
        "created_at_epoch": at,
        "origin": r.get::<_, String>(14)?,
    });
    let mut v = base.as_object().cloned().unwrap_or_default();
    if kind == "summary" {
        v.insert("itemType".into(), json!("summary"));
        v.extend(summary_fields(r.get(3)?, r.get(5)?, r.get(10)?));
    } else {
        v.insert("itemType".into(), json!("observation"));
        v.insert(
            "type".into(),
            json!(
                r.get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "discovery".into())
            ),
        );
        v.insert("title".into(), json!(r.get::<_, Option<String>>(3)?));
        v.insert("subtitle".into(), json!(r.get::<_, Option<String>>(4)?));
        v.insert("narrative".into(), json!(r.get::<_, Option<String>>(5)?));
        v.insert("facts".into(), json_list(r.get(6)?));
        v.insert("concepts".into(), json_list(r.get(7)?));
        v.insert("files_read".into(), json_list(r.get(8)?));
        v.insert("files_modified".into(), json_list(r.get(9)?));
    }
    Ok((at, Value::Object(v)))
}

/// Observations, summaries and human prompts. Without a search: newest first, paged by
/// timestamp (`before` pages backwards, inclusive, and the client drops duplicates at the
/// boundary; `after` returns only newer items for live updates). With a search: memories
/// by relevance (words and meaning), then matching prompts newest first, paged by
/// `offset`; each item says what matched it.
fn feed(conn: &Connection, q: &HashMap<String, String>) -> Result<Value> {
    let limit: i64 = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(40)
        .clamp(1, 200);
    let project = q.get("project").filter(|p| !p.is_empty());
    let text = q.get("q").map(|s| s.trim()).unwrap_or_default();
    let query = Some(fts_query(text)).filter(|s| !s.is_empty());
    let before: Option<i64> = q.get("before").and_then(|v| v.parse().ok());
    let after: Option<i64> = q.get("after").and_then(|v| v.parse().ok());
    if query.is_some() && after.is_none() {
        let offset: i64 = q
            .get("offset")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
            .max(0);
        return ranked_feed(conn, text, project, limit, offset);
    }
    let order = if after.is_some() { "ASC" } else { "DESC" };

    let mut items: Vec<(i64, Value)> = Vec::new();
    let mut sql = format!("SELECT {MEMORY_COLS} FROM memories m");
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut wh: Vec<&str> = Vec::new();
    if let Some(p) = project {
        wh.push("m.project = ?");
        args.push(Box::new(p.clone()));
    }
    if let Some(b) = before {
        wh.push("m.created_at <= ?");
        args.push(Box::new(b));
    }
    if let Some(a) = after {
        wh.push("m.created_at > ?");
        args.push(Box::new(a));
    }
    if !wh.is_empty() {
        sql.push_str(&format!(" WHERE {}", wh.join(" AND ")));
    }
    sql.push_str(&format!(" ORDER BY m.created_at {order} LIMIT ?"));
    args.push(Box::new(limit));
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query(params_from_iter(args.iter().map(|b| b.as_ref())))?;
    while let Some(r) = rows.next()? {
        items.push(memory_item(r)?);
    }
    drop(rows);
    items.extend(prompt_items(
        conn, None, project, before, after, order, limit, 0,
    )?);
    items.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    items.truncate(limit as usize);
    let next_before = (after.is_none() && items.len() as i64 == limit)
        .then(|| items.last().map(|(t, _)| *t))
        .flatten();
    Ok(json!({
        "items": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "next_before": next_before,
    }))
}

/// A page of search results: memories by relevance, then prompts with every word.
fn ranked_feed(
    conn: &Connection,
    text: &str,
    project: Option<&String>,
    limit: i64,
    offset: i64,
) -> Result<Value> {
    let vq = crate::embed::shared().map(|e| e.query(text));
    let (filter, args): (&str, &dyn Fn() -> Vec<Box<dyn ToSql>>) = match project {
        Some(p) => ("m.project = ?", &move || vec![Box::new(p.clone())]),
        None => ("1", &Vec::new),
    };
    let ranked = crate::search::rank_memories(conn, text, vq.as_ref(), filter, args)?;
    let total = ranked.len() as i64;
    let mut row = conn.prepare(&format!(
        "SELECT {MEMORY_COLS} FROM memories m WHERE m.id = ?1"
    ))?;
    let mut items = Vec::new();
    for r in ranked.iter().skip(offset as usize).take(limit as usize) {
        let mut rows = row.query([r.id])?;
        if let Some(x) = rows.next()? {
            let mut v = memory_item(x)?.1;
            v["match"] = json!(r.how());
            items.push(v);
        }
    }
    let room = limit - items.len() as i64;
    if room > 0 {
        let skip = (offset - total).max(0);
        for (_, mut v) in prompt_items(
            conn,
            Some(&fts_query(text)),
            project,
            None,
            None,
            "DESC",
            room,
            skip,
        )? {
            v["match"] = json!("words");
            items.push(v);
        }
    }
    let next_offset = (items.len() as i64 == limit).then_some(offset + limit);
    Ok(json!({ "items": items, "next_before": null, "next_offset": next_offset }))
}

/// Human prompts from transcripts (and imported history).
#[allow(clippy::too_many_arguments)]
fn prompt_items(
    conn: &Connection,
    fq: Option<&str>,
    project: Option<&String>,
    before: Option<i64>,
    after: Option<i64>,
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<(i64, Value)>> {
    let mut sql = String::from(
        "SELECT e.id, e.text, s.project, s.agent, coalesce(e.ts, 0)
         FROM events e JOIN sessions s ON s.id = e.session_id",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut wh: Vec<&str> = vec!["e.kind = 'prompt'", "e.label IS NULL", "e.thread IS NULL"];
    if let Some(fq) = fq {
        sql.push_str(" JOIN events_fts ON events_fts.rowid = e.id");
        wh.push("events_fts MATCH ?");
        args.push(Box::new(fq.to_string()));
    }
    if let Some(p) = project {
        wh.push("s.project = ?");
        args.push(Box::new(p.clone()));
    }
    if let Some(b) = before {
        wh.push("e.ts <= ?");
        args.push(Box::new(b));
    }
    if let Some(a) = after {
        wh.push("e.ts > ?");
        args.push(Box::new(a));
    }
    sql.push_str(&format!(
        " WHERE {} ORDER BY e.ts {order} LIMIT ? OFFSET ?",
        wh.join(" AND ")
    ));
    args.push(Box::new(limit));
    args.push(Box::new(offset));
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query(params_from_iter(args.iter().map(|b| b.as_ref())))?;
    let mut items = Vec::new();
    while let Some(r) = rows.next()? {
        let at: i64 = r.get(4)?;
        items.push((
            at,
            json!({
                "itemType": "prompt",
                "id": r.get::<_, i64>(0)?,
                "prompt_text": r.get::<_, Option<String>>(1)?,
                "project": r.get::<_, Option<String>>(2)?,
                "platform_source": r.get::<_, String>(3)?,
                "created_at_epoch": at,
            }),
        ));
    }
    Ok(items)
}

fn projects(conn: &Connection) -> Result<Value> {
    let mut st = conn.prepare(
        "SELECT project, count(*) FROM sessions WHERE project IS NOT NULL
         GROUP BY project ORDER BY max(coalesce(last_event_at, 0)) DESC LIMIT 300",
    )?;
    let rows: Vec<Value> = st
        .query_map([], |r| {
            Ok(json!({ "project": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(json!({ "projects": rows }))
}

fn stats(conn: &Connection) -> Result<Value> {
    let (sessions, events, memories, newest): (i64, i64, i64, Option<i64>) = conn.query_row(
        "SELECT (SELECT count(*) FROM sessions), (SELECT coalesce(max(id), 0) FROM events),
                (SELECT count(*) FROM memories), (SELECT max(ts) FROM events)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let files_behind: i64 = conn.query_row(
        "SELECT count(*) FROM sources WHERE excluded = 0 AND missing_since IS NULL AND size_seen > byte_offset",
        [],
        |r| r.get(0),
    )?;
    let (pending, err) = crate::distill::pending(conn)?;
    Ok(json!({
        "sessions": sessions,
        "events": events,
        "memories": memories,
        "newest_event_ago": newest.map(|t| context::ago(db::now_ms() - t)).unwrap_or_else(|| "never".into()),
        "files_behind": files_behind,
        "pending_distill": pending,
        "last_distill_error": err,
        "alerts": crate::health::alerts(conn, crate::health::stuck_files(conn)),
    }))
}

#[cfg(test)]
mod tests {
    use super::{HashMap, Value, decode, parse_query, summary_fields};

    #[test]
    fn decodes_queries() {
        assert_eq!(decode("a+b%20c%2Fd"), "a b c/d");
        assert_eq!(decode("100%"), "100%");
        let q = parse_query("project=github.com%2Fo%2Fr&q=queue+stalled");
        assert_eq!(q["project"], "github.com/o/r");
        assert_eq!(q["q"], "queue stalled");
    }

    #[test]
    fn summary_from_narrative() {
        let m = summary_fields(
            Some("Fix login".into()),
            Some("Investigated: logs\nNext steps: ship it".into()),
            None,
        );
        assert_eq!(m["request"], "Fix login");
        assert_eq!(m["investigated"], "logs");
        assert_eq!(m["next_steps"], "ship it");
    }

    #[test]
    fn search_feed_pages_by_rank_then_prompts() {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        // Partial matches only: no memory holds every word of the question.
        for id in 1..=5 {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, origin, origin_id, created_at)
                 VALUES (?1, 'p', 'observation', 'bugfix', ?2, 'mnem', ?1, ?1)",
                rusqlite::params![id, format!("watcher restart note {id}")],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, project) VALUES ('pi:s', 'pi', 's', 'p')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('pi:s', 'k', 9, 'prompt', 'why does the watcher keep restarting')",
            [],
        )
        .unwrap();
        let page = |offset: i64| {
            let q: HashMap<String, String> = [
                ("q", "why does the watcher keep restarting"),
                ("limit", "3"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain([("offset".to_string(), offset.to_string())])
            .collect();
            super::feed(&c, &q).unwrap()
        };
        let (a, b) = (page(0), page(3));
        let keys = |v: &Value| -> Vec<String> {
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| format!("{}{}", i["itemType"].as_str().unwrap(), i["id"]))
                .collect()
        };
        assert_eq!(keys(&a).len(), 3);
        assert_eq!(a["next_offset"], 3);
        let mut all = keys(&a);
        all.extend(keys(&b));
        assert_eq!(all.len(), 6, "{all:?}");
        assert_eq!(all.last().unwrap(), "prompt1");
        assert!(a["items"][0]["match"] == "words");
    }
}
