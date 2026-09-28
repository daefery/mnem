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

pub fn serve(db_path: PathBuf, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("mnem ui: http://127.0.0.1:{port}/  (Ctrl-C to stop)");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let db_path = db_path.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &db_path, port) {
                eprintln!("mnem ui: {e:#}");
            }
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
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
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
            let text = q.get("q").map(String::as_str).unwrap_or_default();
            match crate::embed::shared() {
                Some(e) => {
                    let query = e.query(text);
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

/// Observations, summaries and human prompts, newest first, paged by timestamp.
/// `before` pages backwards (inclusive; the client drops duplicates at the boundary),
/// `after` returns only newer items for live updates.
fn feed(conn: &Connection, q: &HashMap<String, String>) -> Result<Value> {
    let limit: i64 = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(40)
        .clamp(1, 200);
    let project = q.get("project").filter(|p| !p.is_empty());
    let query = q.get("q").map(|s| fts_query(s)).filter(|s| !s.is_empty());
    let before: Option<i64> = q.get("before").and_then(|v| v.parse().ok());
    let after: Option<i64> = q.get("after").and_then(|v| v.parse().ok());
    let order = if after.is_some() { "ASC" } else { "DESC" };

    let mut items: Vec<(i64, Value)> = Vec::new();

    // Memories.
    let mut sql = String::from(
        "SELECT m.id, m.kind, m.type, m.title, m.subtitle, m.narrative, m.facts, m.concepts, m.files_read,
                m.files_modified, m.data, m.project, m.session_id, coalesce(m.created_at, 0), m.origin
         FROM memories m",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut wh: Vec<&str> = Vec::new();
    if let Some(fq) = &query {
        sql.push_str(" JOIN memories_fts ON memories_fts.rowid = m.id");
        wh.push("memories_fts MATCH ?");
        args.push(Box::new(fq.clone()));
    }
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
        items.push((at, Value::Object(v)));
    }
    drop(rows);

    // Human prompts from transcripts (and imported history).
    let mut sql = String::from(
        "SELECT e.id, e.text, s.project, s.agent, coalesce(e.ts, 0)
         FROM events e JOIN sessions s ON s.id = e.session_id",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut wh: Vec<&str> = vec!["e.kind = 'prompt'", "e.label IS NULL", "e.thread IS NULL"];
    if let Some(fq) = &query {
        sql.push_str(" JOIN events_fts ON events_fts.rowid = e.id");
        wh.push("events_fts MATCH ?");
        args.push(Box::new(fq.clone()));
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
        " WHERE {} ORDER BY e.ts {order} LIMIT ?",
        wh.join(" AND ")
    ));
    args.push(Box::new(limit));
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query(params_from_iter(args.iter().map(|b| b.as_ref())))?;
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

    items.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    items.truncate(limit as usize);
    let next_before = (after.is_none() && items.len() as i64 == limit)
        .then(|| items.last().map(|(t, _)| *t))
        .flatten();
    Ok(
        json!({ "items": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>(), "next_before": next_before }),
    )
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
    use super::{decode, parse_query, summary_fields};

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
}
