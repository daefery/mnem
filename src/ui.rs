//! `mnem ui`: a local web viewer for the memory database.
//!
//! A deliberately small HTTP/1.1 server (one thread per connection, no framework). It
//! binds to 127.0.0.1 and rejects requests whose Host header is not local, so other
//! sites in the browser cannot read memory via DNS rebinding. The few actions that
//! change anything (take a backup, import one) are POSTs that must carry an `X-Mnem`
//! header and come from the viewer's own origin: a page on another site cannot send
//! that header without a CORS preflight, which this server never grants.

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

/// Largest backup the viewer accepts for import.
const MAX_UPLOAD: u64 = 32 << 30;

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    cache: bool,
    /// Stream this file as the body (a download) instead of `body`.
    file: Option<PathBuf>,
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
            file: None,
        }
    }
    fn error(status: &'static str, message: impl std::fmt::Display) -> Response {
        Response::text(
            status,
            "application/json; charset=utf-8",
            json!({ "error": message.to_string() }).to_string(),
        )
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
        return send(
            &mut stream,
            Response::text("414 URI Too Long", "text/plain", "request too large\n"),
            false,
            None,
        );
    }
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        header_bytes += n;
        if n == 0 || line == "\r\n" || line == "\n" || header_bytes > 16 * 1024 {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let host = headers.get("host").cloned().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
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
    } else if method == "GET" {
        route(path, &parse_query(query), db_path).unwrap_or_else(|e| {
            Response::text(
                "500 Internal Server Error",
                "text/plain",
                format!("{e:#}\n"),
            )
        })
    } else if method == "POST" {
        if let Err(why) = same_origin(&headers, &host) {
            Response::error("403 Forbidden", why)
        } else {
            // Body bytes already read past the head, then the rest from the socket.
            let early = reader.buffer().to_vec();
            drop(reader);
            let body = Body {
                early,
                stream: &mut stream,
                length: headers.get("content-length").and_then(|v| v.parse().ok()),
            };
            post(path, &parse_query(query), body, db_path)
                .unwrap_or_else(|e| Response::error("500 Internal Server Error", format!("{e:#}")))
        }
    } else {
        Response::text("405 Method Not Allowed", "text/plain", "GET or POST only\n")
    };
    let gzip = headers
        .get("accept-encoding")
        .is_some_and(|v| accepts_gzip(v));
    send(
        &mut stream,
        resp,
        gzip,
        headers.get("if-none-match").map(String::as_str),
    )
}

/// Whether an Accept-Encoding value allows gzip (not refused with q=0).
fn accepts_gzip(v: &str) -> bool {
    v.split(',').any(|e| {
        let mut p = e.split(';').map(str::trim);
        p.next().is_some_and(|n| n.eq_ignore_ascii_case("gzip"))
            && !p.any(|q| q.replace(' ', "").eq_ignore_ascii_case("q=0"))
    })
}

/// Bodies smaller than this are sent as they are (compressing them saves nothing).
const GZIP_MIN: usize = 1024;

/// A strong validator for a body: its hash, quoted.
fn etag(body: &[u8]) -> String {
    format!("\"{:016x}\"", xxhash_rust::xxh3::xxh3_64(body))
}

/// Write a response; a file body is streamed, never loaded whole.
/// Write a response; a file body is streamed, never loaded whole. Text bodies are
/// gzipped when the browser accepts it, and successful ones carry an ETag so a reload
/// that already has them gets `304 Not Modified` and no body.
fn send(
    stream: &mut TcpStream,
    mut resp: Response,
    gzip: bool,
    if_none_match: Option<&str>,
) -> Result<()> {
    let mut extra = String::new();
    let mut status = resp.status;
    if resp.file.is_none() && resp.status == "200 OK" {
        let tag = etag(&resp.body);
        if if_none_match.is_some_and(|v| v.split(',').any(|t| t.trim() == tag)) {
            status = "304 Not Modified";
            resp.body.clear();
        }
        extra.push_str(&format!("ETag: {tag}\r\n"));
    }
    let compressible = resp.content_type.starts_with("text/")
        || resp.content_type.starts_with("application/json")
        || resp.content_type.starts_with("image/svg");
    if resp.file.is_none() && compressible {
        // Caches keep the plain and the gzipped body apart.
        extra.push_str("Vary: Accept-Encoding\r\n");
        if gzip && resp.body.len() >= GZIP_MIN {
            use flate2::{Compression, write::GzEncoder};
            let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
            enc.write_all(&resp.body)?;
            resp.body = enc.finish()?;
            extra.push_str("Content-Encoding: gzip\r\n");
        }
    }
    let (length, disposition) = match &resp.file {
        Some(f) => (
            f.metadata()?.len(),
            format!(
                "Content-Disposition: attachment; filename=\"{}\"\r\n",
                f.file_name().unwrap_or_default().to_string_lossy()
            ),
        ),
        None => (resp.body.len() as u64, String::new()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{disposition}{extra}Cache-Control: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        resp.content_type,
        length,
        if resp.cache {
            "max-age=86400"
        } else {
            "no-cache"
        }
    );
    stream.write_all(head.as_bytes())?;
    match &resp.file {
        Some(f) => {
            std::io::copy(&mut std::fs::File::open(f)?, stream)?;
        }
        None => stream.write_all(&resp.body)?,
    }
    Ok(())
}

/// Actions that change something must come from the viewer itself: a custom header
/// (another site's page cannot add one without a preflight this server never answers)
/// and, when the browser says where the request comes from, this very origin.
fn same_origin(
    headers: &HashMap<String, String>,
    host: &str,
) -> std::result::Result<(), &'static str> {
    if headers.get("x-mnem").map(String::as_str) != Some("1") {
        return Err("missing X-Mnem header");
    }
    if let Some(origin) = headers.get("origin")
        && origin != &format!("http://{host}")
    {
        return Err("request from another origin");
    }
    if let Some(site) = headers.get("sec-fetch-site")
        && site != "same-origin"
        && site != "none"
    {
        return Err("request from another site");
    }
    Ok(())
}

/// A request body: bytes that arrived with the head, then the rest of the socket.
struct Body<'a> {
    early: Vec<u8>,
    stream: &'a mut TcpStream,
    length: Option<u64>,
}

impl Body<'_> {
    /// Write exactly Content-Length bytes to `out`.
    fn save(self, out: &mut impl Write, limit: u64) -> Result<u64> {
        let length = self
            .length
            .ok_or_else(|| anyhow::anyhow!("Content-Length required"))?;
        anyhow::ensure!(length <= limit, "upload larger than {limit} bytes");
        let first = self.early.len().min(length as usize);
        out.write_all(&self.early[..first])?;
        // A whole-upload deadline, so a slow drip cannot hold the only upload slot:
        // 30 s plus one second per 10 MB (a 440 MB backup gets 74 s; a browser on the
        // same machine sends it in a few). The caller removes the partial file.
        self.save_within(
            out,
            length,
            Duration::from_secs(30 + length / 10_000_000),
            first,
        )
    }

    fn save_within(
        self,
        out: &mut impl Write,
        length: u64,
        allowed: Duration,
        first: usize,
    ) -> Result<u64> {
        let deadline = std::time::Instant::now() + allowed;
        let mut done = first as u64;
        let mut buf = vec![0u8; 1 << 20];
        let too_slow =
            |done: u64| anyhow::anyhow!("upload too slow; stopped after {done} of {length} bytes");
        while done < length {
            // No read may wait past the deadline.
            let left = deadline
                .checked_duration_since(std::time::Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| too_slow(done))?;
            self.stream
                .set_read_timeout(Some(left.min(Duration::from_secs(10))))?;
            let want = buf.len().min((length - done) as usize);
            let n = match std::io::Read::read(self.stream, &mut buf[..want]) {
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) && std::time::Instant::now() >= deadline =>
                {
                    return Err(too_slow(done));
                }
                Err(e) => return Err(e.into()),
            };
            anyhow::ensure!(n > 0, "upload ended early ({done} of {length} bytes)");
            out.write_all(&buf[..n])?;
            done += n as u64;
        }
        if std::time::Instant::now() > deadline {
            return Err(too_slow(done));
        }
        Ok(length)
    }
}

fn route(path: &str, q: &HashMap<String, String>, db_path: &Path) -> Result<Response> {
    Ok(match path {
        "/" | "/index.html" => Response::text("200 OK", "text/html; charset=utf-8", INDEX),
        "/app.js" => Response::text("200 OK", "text/javascript; charset=utf-8", APP),
        "/style.css" => Response::text("200 OK", "text/css; charset=utf-8", STYLE),
        "/logo.svg" => Response::text("200 OK", "image/svg+xml", LOGO),
        p if p.starts_with("/icons/") => match ICONS.iter().find(|(n, _)| p.ends_with(n)) {
            Some((_, svg)) => Response {
                status: "200 OK",
                content_type: "image/svg+xml",
                body: svg.as_bytes().to_vec(),
                cache: true,
                file: None,
            },
            None => not_found(),
        },
        "/api/backups" => Response::json(&backups(&open(db_path)?, db_path)?),
        p if p.starts_with("/api/backups/") => download(p),
        "/api/feed" => Response::json(&feed(&open(db_path)?, q)?),
        "/api/projects" => Response::json(&projects(&open(db_path)?)?),
        "/api/stats" => Response::json(&stats(&open(db_path)?)?),
        "/api/agents" => Response::json(&json!({ "agents": crate::agents::status_all() })),
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

/// Uploaded backups wait here (outside the rotated snapshots) until applied or discarded.
fn incoming() -> PathBuf {
    crate::backup::dir().join("incoming")
}

/// A file name the viewer may serve or act on: produced by mnem, no path parts.
fn safe_name<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    (name.starts_with(prefix)
        && name.ends_with(".db")
        && name.len() < 80
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !name.contains(".."))
    .then_some(name)
}

fn counts(conn: &Connection, db_path: &Path) -> Result<Value> {
    let (sessions, events, memories): (i64, i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM events), (SELECT count(*) FROM memories)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(json!({
        "sessions": sessions, "events": events, "memories": memories,
        "bytes": db_path.metadata().map(|m| m.len()).unwrap_or(0),
        "host": crate::backup::hostname(),
    }))
}

/// This machine's memory and its snapshots, newest first.
fn backups(conn: &Connection, db_path: &Path) -> Result<Value> {
    let list: Vec<Value> = crate::backup::list(&crate::backup::dir())?
        .into_iter()
        .filter_map(|(path, m)| {
            let m = m?;
            let origin = crate::backup::origin(&path).unwrap_or_default();
            Some(json!({
                "file": m.file, "created_at": m.created_at, "bytes": m.bytes,
                "sessions": m.sessions, "events": m.events, "memories": m.memories,
                "host": origin.host, "has_settings": origin.config.is_some(),
            }))
        })
        .collect();
    Ok(
        json!({ "current": counts(conn, db_path)?, "backups": list, "can_restart": under_service(),
                "auto": crate::backup::auto_enabled(conn) }),
    )
}

/// Stream a verified snapshot as a download.
fn download(path: &str) -> Response {
    let name = path.trim_start_matches("/api/backups/");
    match safe_name(name, "mnem-") {
        Some(n) if crate::backup::dir().join(n).is_file() => Response {
            status: "200 OK",
            content_type: "application/vnd.sqlite3",
            body: vec![],
            cache: false,
            file: Some(crate::backup::dir().join(n)),
        },
        _ => not_found(),
    }
}

/// The viewer's actions: take a backup, upload one, restore it, or restart mnem.
fn post(path: &str, q: &HashMap<String, String>, body: Body, db_path: &Path) -> Result<Response> {
    Ok(match path {
        // The daily automatic backup, on or off.
        "/api/backups/auto" => {
            let on = match q.get("on").map(String::as_str) {
                Some("1") => true,
                Some("0") => false,
                _ => return Ok(Response::error("400 Bad Request", "on must be 1 or 0")),
            };
            let conn = open(db_path)?;
            crate::backup::set_auto(&conn, on)?;
            Response::json(&json!({ "auto": crate::backup::auto_enabled(&conn) }))
        }
        "/api/backups" => {
            let m =
                crate::backup::create(&open(db_path)?, &crate::backup::dir(), crate::backup::KEEP)?;
            Response::json(&json!({ "backup": m }))
        }
        "/api/import" => {
            static UPLOADING: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if UPLOADING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Ok(Response::error(
                    "409 Conflict",
                    "another import is being uploaded",
                ));
            }
            struct Done;
            impl Drop for Done {
                fn drop(&mut self) {
                    UPLOADING.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _done = Done;
            let dir = incoming();
            std::fs::create_dir_all(&dir)?;
            // The upload, its checked copy and a snapshot of the current memory all sit on
            // this disk at once.
            let length = body.length.unwrap_or(0);
            let live = db_path.metadata().map(|m| m.len()).unwrap_or(0);
            let need = 2 * length + live + (64 << 20);
            if let Some(free) = crate::backup::free_bytes(&dir)
                && free < need
            {
                return Ok(Response::error(
                    "507 Insufficient Storage",
                    format!(
                        "importing this backup needs about {} MB free next to mnem's data; {} MB are free",
                        need / 1_000_000,
                        free / 1_000_000
                    ),
                ));
            }
            // One pending import at a time: older uploads are dropped.
            for e in std::fs::read_dir(&dir)?.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
            let name = format!("upload-{}.db", db::now_ms());
            let partial = dir.join(format!("{name}.partial"));
            let saved = (|| -> Result<()> {
                let mut f = std::io::BufWriter::new(std::fs::File::create(&partial)?);
                body.save(&mut f, MAX_UPLOAD)?;
                f.flush()?;
                std::fs::rename(&partial, dir.join(&name))?;
                Ok(())
            })();
            if let Err(e) = saved {
                let _ = std::fs::remove_file(&partial);
                return Ok(Response::error("400 Bad Request", format!("{e:#}")));
            }
            match crate::backup::check_import(&dir.join(&name)) {
                Ok((m, origin)) => Response::json(&json!({
                    "settings": crate::backup::review_settings(&dir.join(&name)).ok().flatten(),
                    "file": name,
                    "backup": { "sessions": m.sessions, "events": m.events, "memories": m.memories,
                                "bytes": m.bytes, "schema": m.schema_version },
                    "origin": { "host": origin.host, "taken_at": origin.taken_at,
                                "version": origin.version, "has_settings": origin.config.is_some() },
                    "current": counts(&open(db_path)?, db_path)?,
                })),
                Err(e) => {
                    let _ = std::fs::remove_file(dir.join(&name));
                    Response::error("422 Unprocessable Content", format!("{e:#}"))
                }
            }
        }
        "/api/import/apply" => {
            // Every answer says whether this machine's memory changed.
            let fail = |status, changed: bool, msg: String| {
                Response::text(
                    status,
                    "application/json; charset=utf-8",
                    json!({ "error": msg, "changed": changed }).to_string(),
                )
            };
            let Some(name) = q.get("file").and_then(|f| safe_name(f, "upload-")) else {
                return Ok(fail("400 Bad Request", false, "no such upload".into()));
            };
            let file = incoming().join(name);
            if !file.is_file() {
                return Ok(fail(
                    "404 Not Found",
                    false,
                    "the upload is gone; upload it again".into(),
                ));
            }
            let want_settings = q.get("settings").map(String::as_str) == Some("1");
            // Settings are checked before anything changes.
            if want_settings {
                match crate::backup::review_settings(&file) {
                    Ok(Some(r)) if r.valid => {}
                    Ok(Some(r)) => {
                        return Ok(fail(
                            "422 Unprocessable Content",
                            false,
                            format!(
                                "the backup's settings are not valid ({}); import without them",
                                r.error.unwrap_or_default()
                            ),
                        ));
                    }
                    Ok(None) => {
                        return Ok(fail(
                            "422 Unprocessable Content",
                            false,
                            "the backup carries no settings".into(),
                        ));
                    }
                    Err(e) => {
                        return Ok(fail("500 Internal Server Error", false, format!("{e:#}")));
                    }
                }
            }
            let origin = crate::backup::origin(&file).unwrap_or_default();
            let mut conn = db::open(db_path)?;
            if let Err(e) = crate::backup::restore(&file, &mut conn, &crate::backup::dir()) {
                return Ok(fail("500 Internal Server Error", false, format!("{e:#}")));
            }
            let _ = std::fs::remove_file(&file);
            // From here on the memory has been replaced; report the rest honestly.
            let foreign = if origin
                .host
                .as_deref()
                .is_some_and(|h| h != crate::backup::hostname())
            {
                crate::backup::mark_foreign_sources(&conn).unwrap_or(0)
            } else {
                0
            };
            let (settings, settings_error) = if want_settings {
                match crate::backup::apply_settings_file(&file_settings(&origin)) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(format!("{e:#}"))),
                }
            } else {
                (false, None)
            };
            Response::json(&json!({
                "changed": true,
                "settings_applied": settings,
                "settings_error": settings_error,
                "foreign_transcripts": foreign,
                "can_restart": under_service(),
                "current": counts(&conn, db_path)?,
            }))
        }
        "/api/import/discard" => {
            if let Some(name) = q.get("file").and_then(|f| safe_name(f, "upload-")) {
                let _ = std::fs::remove_file(incoming().join(name));
            }
            Response::json(&json!({ "discarded": true }))
        }
        // Connect one agent: what `mnem install --only <agent>` does, reported back.
        "/api/agents/connect" => {
            let Some(agent) = q.get("agent").and_then(|a| crate::agents::Agent::parse(a)) else {
                return Ok(Response::error("400 Bad Request", "unknown agent"));
            };
            let plan = crate::install::Plan {
                bin: crate::install::default_bin(),
                dry_run: false,
                claude: agent == crate::agents::Agent::Claude,
                codex: agent == crate::agents::Agent::Codex,
                pi: agent == crate::agents::Agent::Pi,
                watch: false,
            };
            let (result, log) = crate::install::collecting(|| crate::install::run(&plan));
            let status = crate::agents::status(agent);
            match result {
                Ok(()) => Response::json(&json!({ "status": status, "log": log })),
                Err(e) => Response::text(
                    "500 Internal Server Error",
                    "application/json; charset=utf-8",
                    json!({ "error": format!("{e:#}"), "status": status, "log": log }).to_string(),
                ),
            }
        }
        "/api/restart" => {
            if !under_service() {
                return Ok(Response::error(
                    "409 Conflict",
                    "mnem is not running under a systemd unit that restarts it: restart mnem yourself",
                ));
            }
            // systemd (Restart=always) starts mnem again; answer first, then exit.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                // EX_TEMPFAIL: Restart=always and on-failure both start mnem again.
                std::process::exit(75);
            });
            Response::json(&json!({ "restarting": true }))
        }
        _ => not_found(),
    })
}

/// The settings text an origin carried (checked before the restore began).
fn file_settings(origin: &crate::backup::Origin) -> String {
    origin.config.clone().unwrap_or_default()
}

/// True when systemd supervises this process (it sets INVOCATION_ID) and its unit
/// restarts it after the exit code the restart action uses.
fn under_service() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
        && restart_policy().is_some_and(|p| p == "always" || p == "on-failure")
}

/// The Restart= policy of the systemd unit running this process, read from its cgroup.
fn restart_policy() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let unit = cgroup
        .lines()
        .flat_map(|l| l.rsplit('/'))
        .find(|p| p.ends_with(".service"))?
        .to_string();
    let mut cmd = std::process::Command::new("systemctl");
    if cgroup.contains("/user@") {
        cmd.arg("--user");
    }
    let out = cmd
        .args(["show", "-p", "Restart", "--value", &unit])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    // Ties in time: the order SQLite used before the time index existed, which the
    // viewer showed until then (for one project, by its project index: newest id first;
    // across projects, by a sort in table order: oldest id first).
    let tie = if project.is_some() { "DESC" } else { "ASC" };
    sql.push_str(&format!(
        " ORDER BY m.created_at {order}, m.id {tie} LIMIT ?"
    ));
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
    // Newer than `after`: the oldest `limit` of them (shown newest first), so a viewer
    // catching up after a while pages forward without skipping any; `more` says
    // another page is waiting. Otherwise the newest `limit`.
    if after.is_some() {
        items.sort_by_key(|(t, _)| *t);
        items.truncate(limit as usize);
        items.reverse();
    } else {
        items.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        items.truncate(limit as usize);
    }
    let full = items.len() as i64 == limit;
    let next_before = (after.is_none() && full)
        .then(|| items.last().map(|(t, _)| *t))
        .flatten();
    Ok(json!({
        "items": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "next_before": next_before,
        "more": after.is_some() && full,
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
    // Prompts at the same millisecond (different sessions) are rare; the one project
    // plan leaves them in its own order, which no tiebreak can reproduce, so the new
    // index's order is kept there (the all-projects feed is unchanged: oldest id first).
    sql.push_str(&format!(
        " WHERE {} ORDER BY e.ts {order}, e.id ASC LIMIT ? OFFSET ?",
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
    use super::{Body, HashMap, Value, decode, parse_query, summary_fields};

    #[test]
    fn a_dripping_upload_is_stopped_at_its_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let drip = std::thread::spawn(move || {
            use std::io::Write;
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            for _ in 0..40 {
                if c.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let body = Body {
            early: vec![],
            stream: &mut stream,
            length: Some(1000),
        };
        let t = std::time::Instant::now();
        let r = body.save_within(
            &mut Vec::new(),
            1000,
            std::time::Duration::from_millis(500),
            0,
        );
        assert!(r.unwrap_err().to_string().contains("too slow"));
        // Stopped at the deadline, not up to a read timeout later.
        assert!(
            t.elapsed() < std::time::Duration::from_millis(800),
            "{:?}",
            t.elapsed()
        );
        drop(stream);
        let _ = drip.join();
    }

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
    fn gzip_is_offered_only_when_accepted() {
        assert!(super::accepts_gzip("gzip, deflate, br"));
        assert!(super::accepts_gzip("br;q=1.0, GZIP;q=0.8"));
        assert!(!super::accepts_gzip("gzip;q=0, deflate"));
        assert!(!super::accepts_gzip("deflate, br"));
        assert!(!super::accepts_gzip("x-gzip-not"));
    }

    /// The feed's two lists come from indexes, not a sort of every row.
    #[test]
    fn feed_queries_use_their_indexes() {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        let plan = |sql: &str| -> String {
            let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            st.query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>()
                .join(" / ")
        };
        let m = plan(
            "SELECT m.id FROM memories m WHERE m.created_at <= 5 ORDER BY m.created_at DESC LIMIT 40",
        );
        assert!(
            m.contains("memories_time") && !m.contains("TEMP B-TREE"),
            "{m}"
        );
        let p = plan(
            "SELECT e.id FROM events e JOIN sessions s ON s.id = e.session_id
              WHERE e.kind = 'prompt' AND e.label IS NULL AND e.thread IS NULL
              ORDER BY e.ts DESC LIMIT 40 OFFSET 0",
        );
        assert!(
            p.contains("events_prompts") && !p.contains("TEMP B-TREE"),
            "{p}"
        );
    }

    /// Catching up after a while: the oldest new items first, then the next page.
    #[test]
    fn live_updates_page_forward_without_skipping() {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, project) VALUES ('pi:s', 'pi', 's', 'p')",
            [],
        )
        .unwrap();
        for (id, t) in [(1, 201), (2, 202), (3, 203)] {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, origin, origin_id, created_at)
                 VALUES (?1, 'p', 'observation', 'change', 't', 'mnem', ?1, ?2)",
                [id, t],
            )
            .unwrap();
        }
        for t in [301, 302, 303] {
            c.execute(
                "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('pi:s', ?1, ?2, 'prompt', 'x')",
                rusqlite::params![format!("k{t}"), t],
            )
            .unwrap();
        }
        let q = |after: &str| -> Value {
            let q: HashMap<String, String> = [("limit", "3"), ("after", after)]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            super::feed(&c, &q).unwrap()
        };
        let first = q("100");
        let ids: Vec<i64> = first["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, [3, 2, 1], "the three oldest new ones, newest first");
        assert_eq!(first["more"], true);
        let second = q("203");
        assert_eq!(second["items"].as_array().unwrap().len(), 3);
        assert!(
            second["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|i| i["itemType"] == "prompt")
        );
        assert_eq!(q("303")["more"], false);
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
