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
const FONT: &[u8] = include_bytes!("../ui/fonts/monaspace-radon-var.woff2");

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
    send(&mut stream, resp)
}

/// Write a response; a file body is streamed, never loaded whole.
fn send(stream: &mut TcpStream, resp: Response) -> Result<()> {
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
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{disposition}Cache-Control: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        resp.status,
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
        "/fonts/monaspace-radon-var.woff2" => Response {
            status: "200 OK",
            content_type: "font/woff2",
            body: FONT.to_vec(),
            cache: true,
            file: None,
        },
        "/api/backups" => Response::json(&backups(&open(db_path)?, db_path)?),
        p if p.starts_with("/api/backups/") => download(p),
        "/api/feed" => Response::json(&feed(&open(db_path)?, q)?),
        p if p.starts_with("/api/memory/") => match p["/api/memory/".len()..].parse::<i64>().ok() {
            Some(id) => match memory_detail(&open(db_path)?, id)? {
                Some(v) => Response::json(&v),
                None => not_found(),
            },
            None => not_found(),
        },
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
        json!({ "current": counts(conn, db_path)?, "backups": list, "can_restart": under_service() }),
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
        "pinned": kind == "pinned",
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
/// What the feed shows, from the query string: `project`, `type` (comma-separated
/// observation types, plus `summary`, `pinned` and `prompt`), `agent` (claude, codex,
/// pi) and `view` (a question the viewer answers: `pinned`, `unopened` for memories
/// offered to agents but never fetched in full, `edited` for memories whose session
/// changed files). Every condition is over `memories m`; its values are bound, never
/// spliced into the SQL.
#[derive(Debug, Default)]
struct Filters {
    project: Option<String>,
    types: Vec<String>,
    agent: Option<String>,
    view: Option<&'static str>,
}

impl Filters {
    fn from(q: &HashMap<String, String>) -> Filters {
        let types = q
            .get("type")
            .map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Filters {
            project: q.get("project").filter(|p| !p.is_empty()).cloned(),
            types,
            agent: q
                .get("agent")
                .filter(|a| !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric()))
                .cloned(),
            view: q.get("view").and_then(|v| {
                ["pinned", "unopened", "edited"]
                    .into_iter()
                    .find(|k| k == v)
            }),
        }
    }

    /// The SQL condition on memories and its arguments ("1" when nothing filters).
    fn memories(&self) -> (String, Vec<String>) {
        let mut wh: Vec<String> = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(p) = &self.project {
            wh.push("m.project = ?".into());
            args.push(p.clone());
        }
        if !self.types.is_empty() {
            let special = ["summary", "pinned", "prompt"];
            let obs: Vec<&String> = self
                .types
                .iter()
                .filter(|t| !special.contains(&t.as_str()))
                .collect();
            let mut any: Vec<String> = Vec::new();
            if !obs.is_empty() {
                any.push(format!(
                    "(m.kind = 'observation' AND m.type IN ({}))",
                    vec!["?"; obs.len()].join(", ")
                ));
                args.extend(obs.into_iter().cloned());
            }
            for kind in ["summary", "pinned"] {
                if self.types.iter().any(|t| t == kind) {
                    any.push(format!("m.kind = '{kind}'"));
                }
            }
            wh.push(if any.is_empty() {
                "0".into()
            } else {
                format!("({})", any.join(" OR "))
            });
        }
        if let Some(a) = &self.agent {
            wh.push("m.session_id LIKE ? || ':%'".into());
            args.push(a.clone());
        }
        match self.view {
            Some("pinned") => wh.push("m.kind = 'pinned'".into()),
            // Driven from the few offered ids, not a scan of every memory.
            Some("unopened") => wh.push(format!(
                "m.id IN (SELECT o.memory_id FROM offers o
                   WHERE NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = o.session_id))
                 AND m.id NOT IN (SELECT j.value FROM {FETCHED})"
            )),
            Some("edited") => wh.push(
                "EXISTS (SELECT 1 FROM memory_files f WHERE f.memory_id = m.id AND f.modified = 1)"
                    .into(),
            ),
            _ => {}
        }
        if wh.is_empty() {
            ("1".into(), args)
        } else {
            (wh.join(" AND "), args)
        }
    }

    /// Whether human prompts belong in this feed: not in a view, and only when no type
    /// is chosen or `prompt` is one of them.
    fn prompts(&self) -> bool {
        self.view.is_none() && (self.types.is_empty() || self.types.iter().any(|t| t == "prompt"))
    }
}

/// Every memory id an agent fetched in full over MCP, as `j.value`.
const FETCHED: &str = "mcp_calls c, json_each(c.ids) j";

/// Longest the detail pane waits on git for its files; the rest are reported unchecked.
const DETAIL_GIT_BUDGET: Duration = Duration::from_secs(3);

fn boxed(args: &[String]) -> Vec<Box<dyn ToSql>> {
    args.iter()
        .map(|a| Box::new(a.clone()) as Box<dyn ToSql>)
        .collect()
}

/// A position in the time-ordered feed: time, then the stream (memories before prompts),
/// then id, so every item has its own place even when many share a millisecond. Pages
/// go strictly before or after one, and nothing is skipped or repeated at a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cursor {
    at: i64,
    stream: i64,
    id: i64,
}

impl Cursor {
    const MEMORY: i64 = 0;
    const PROMPT: i64 = 1;

    /// `at.stream.id`, or a bare time: the last place at that time, so `after` it means
    /// later than that time and `before` it includes that time.
    fn parse(s: &str) -> Option<Cursor> {
        let mut p = s.split('.');
        let at = p.next()?.parse().ok()?;
        match (p.next(), p.next(), p.next()) {
            (Some(k), Some(id), None) => Some(Cursor {
                at,
                stream: k.parse().ok()?,
                id: id.parse().ok()?,
            }),
            (None, _, _) => Some(Cursor {
                at,
                stream: i64::MAX,
                id: i64::MAX,
            }),
            _ => None,
        }
    }

    fn text(&self) -> String {
        format!("{}.{}.{}", self.at, self.stream, self.id)
    }

    /// SQL for "(time, stream, id) is before (`<`) or after (`>`) this cursor", over
    /// the given time and id columns of one stream, and its arguments.
    fn sql(&self, op: &str, at: &str, id: &str, stream: i64) -> (String, Vec<Box<dyn ToSql>>) {
        (
            format!("({at}, ?, {id}) {op} (?, ?, ?)"),
            vec![
                Box::new(stream),
                Box::new(self.at),
                Box::new(self.stream),
                Box::new(self.id),
            ],
        )
    }
}

fn feed(conn: &Connection, q: &HashMap<String, String>) -> Result<Value> {
    let limit: i64 = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(40)
        .clamp(1, 200);
    let filters = Filters::from(q);
    let text = q.get("q").map(|s| s.trim()).unwrap_or_default();
    let query = Some(fts_query(text)).filter(|s| !s.is_empty());
    let before = q.get("before").and_then(|v| Cursor::parse(v));
    let after = q.get("after").and_then(|v| Cursor::parse(v));
    if query.is_some() && after.is_none() {
        let offset: i64 = q
            .get("offset")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
            .max(0);
        return ranked_feed(conn, text, &filters, limit, offset);
    }
    let order = if after.is_some() { "ASC" } else { "DESC" };
    let bound = |at: &str, id: &str, stream: i64| -> (String, Vec<Box<dyn ToSql>>) {
        match (before, after) {
            (_, Some(a)) => a.sql(">", at, id, stream),
            (Some(b), None) => b.sql("<", at, id, stream),
            (None, None) => ("1".into(), vec![]),
        }
    };

    let mut items: Vec<(Cursor, Value)> = Vec::new();
    let (cond, cond_args) = filters.memories();
    let (range, range_args) = bound("coalesce(m.created_at, 0)", "m.id", Cursor::MEMORY);
    let sql = format!(
        "SELECT {MEMORY_COLS} FROM memories m WHERE {cond} AND {range}
          ORDER BY coalesce(m.created_at, 0) {order}, m.id {order} LIMIT ?"
    );
    let mut args = boxed(&cond_args);
    args.extend(range_args);
    args.push(Box::new(limit));
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query(params_from_iter(args.iter().map(|b| b.as_ref())))?;
    while let Some(r) = rows.next()? {
        let (at, v) = memory_item(r)?;
        let id = v["id"].as_i64().unwrap_or(0);
        items.push((
            Cursor {
                at,
                stream: Cursor::MEMORY,
                id,
            },
            v,
        ));
    }
    drop(rows);
    if filters.prompts() {
        let (range, range_args) = bound("coalesce(e.ts, 0)", "e.id", Cursor::PROMPT);
        for (at, v) in prompt_items(conn, None, &filters, (range, range_args), order, limit, 0)? {
            let id = v["id"].as_i64().unwrap_or(0);
            items.push((
                Cursor {
                    at,
                    stream: Cursor::PROMPT,
                    id,
                },
                v,
            ));
        }
    }
    // Newer than `after`: the `limit` nearest to it, so nothing between the client's
    // newest item and the page is skipped (it asks again from the page's newest), shown
    // newest first. Otherwise the newest `limit` before `before`.
    if after.is_some() {
        items.sort_by_key(|(c, _)| *c);
        items.truncate(limit as usize);
        items.reverse();
    } else {
        items.sort_by_key(|(c, _)| std::cmp::Reverse(*c));
        items.truncate(limit as usize);
    }
    let full = items.len() as i64 == limit;
    let next_before = (after.is_none() && full)
        .then(|| items.last().map(|(c, _)| c.text()))
        .flatten();
    // Where live updates continue from: the newest item here, else where they were.
    let newest = items.first().map(|(c, _)| *c).or(after).map(|c| c.text());
    Ok(json!({
        "items": items.into_iter().map(|(c, mut v)| { v["cursor"] = json!(c.text()); v }).collect::<Vec<_>>(),
        "next_before": next_before,
        "newest": newest,
        "more": after.is_some() && full,
    }))
}

/// A page of search results: memories by relevance, then prompts with every word.
fn ranked_feed(
    conn: &Connection,
    text: &str,
    filters: &Filters,
    limit: i64,
    offset: i64,
) -> Result<Value> {
    let vq = crate::embed::shared().map(|e| e.query(text));
    let (filter, args) = filters.memories();
    let ranked = crate::search::rank_memories(conn, text, vq.as_ref(), &filter, &|| boxed(&args))?;
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
    if room > 0 && filters.prompts() {
        let skip = (offset - total).max(0);
        for (_, mut v) in prompt_items(
            conn,
            Some(&fts_query(text)),
            filters,
            ("1".into(), vec![]),
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

/// Human prompts from transcripts (and imported history), in `filters`' project and
/// agent.
#[allow(clippy::too_many_arguments)]
fn prompt_items(
    conn: &Connection,
    fq: Option<&str>,
    filters: &Filters,
    (range, range_args): (String, Vec<Box<dyn ToSql>>),
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
    if let Some(p) = &filters.project {
        wh.push("s.project = ?");
        args.push(Box::new(p.clone()));
    }
    if let Some(a) = &filters.agent {
        wh.push("s.agent = ?");
        args.push(Box::new(a.clone()));
    }
    wh.push(&range);
    args.extend(range_args);
    sql.push_str(&format!(
        " WHERE {} ORDER BY coalesce(e.ts, 0) {order}, e.id {order} LIMIT ? OFFSET ?",
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

/// One memory in full for the viewer's detail pane: the feed item, the files it touched
/// as they are on this machine now (whether its own edits survive), and its uptake
/// (how often it was offered to agents and fetched in full). Reading it here is not an
/// agent using it, so nothing is recorded: uptake stays about agents.
fn memory_detail(conn: &Connection, id: i64) -> Result<Option<Value>> {
    let mut st = conn.prepare(&format!(
        "SELECT {MEMORY_COLS} FROM memories m WHERE m.id = ?1"
    ))?;
    let mut rows = st.query([id])?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    let mut v = memory_item(r)?.1;
    v["session_id"] = json!(r.get::<_, Option<String>>(12)?);
    drop(rows);
    let (files, unchecked) = crate::files::files_now(conn, id, 12, DETAIL_GIT_BUDGET)?;
    let files: Vec<Value> = files
        .into_iter()
        .map(|f| {
            json!({
                "path": f.rel,
                "modified": f.modified,
                "change": f.change,
                "kept": f.kept.map(|k| json!({
                    "kept": k.kept, "of": k.of, "intact": k.intact(), "phrase": k.phrase(),
                })),
            })
        })
        .collect();
    v["files_now"] = json!(files);
    v["files_unchecked"] = json!(unchecked);
    let (offered, sessions, last): (i64, i64, Option<i64>) = conn.query_row(
        "SELECT count(*), count(DISTINCT o.session_id), max(o.at) FROM offers o
          WHERE o.memory_id = ?1
            AND NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = o.session_id)",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let fetched: i64 = conn.query_row(
        // The substring test skips calls that cannot hold the id before any JSON is read.
        &format!(
            "SELECT count(*) FROM {FETCHED} WHERE instr(c.ids, CAST(?1 AS TEXT)) > 0 AND j.value = ?1"
        ),
        [id],
        |r| r.get(0),
    )?;
    v["uptake"] = json!({
        "offered": offered,
        "sessions": sessions,
        "fetched": fetched,
        "last_offered_ago": last.map(|t| context::ago(db::now_ms() - t)),
    });
    v["evidence"] = json!(conn.query_row(
        "SELECT count(*) FROM memory_evidence WHERE memory_id = ?1",
        [id],
        |r| r.get::<_, i64>(0),
    )?);
    Ok(Some(v))
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
    let backup = crate::backup::newest_age(&crate::backup::dir());
    Ok(json!({
        "backup_ago": backup.map(context::ago),
        "backup_stale": backup.is_none_or(|a| a > 2 * crate::backup::INTERVAL_MS),
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

    fn fixture() -> rusqlite::Connection {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        for (id, session, kind, ty) in [
            (1, "claude:a", "observation", "bugfix"),
            (2, "pi:b", "observation", "decision"),
            (3, "claude:a", "summary", ""),
            (4, "codex:c", "observation", "bugfix"),
        ] {
            c.execute(
                "INSERT INTO memories(id, session_id, project, kind, type, title, origin, origin_id, created_at)
                 VALUES (?1, ?2, 'p', ?3, nullif(?4, ''), 'watcher note', 'mnem', ?1, ?1)",
                rusqlite::params![id, session, kind, ty],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO memories(id, project, kind, type, title, narrative, origin, origin_id, created_at)
             VALUES (5, 'p', 'pinned', 'decision', 'keep it local', 'keep it local', 'user', 5, 5)",
            [],
        )
        .unwrap();
        c.execute(
            "UPDATE memories SET files_modified = '[\"src/a.rs\"]' WHERE id = 2",
            [],
        )
        .unwrap();
        for (id, agent) in [("claude:a", "claude"), ("pi:b", "pi")] {
            c.execute(
                "INSERT INTO sessions(id, agent, native_id, project) VALUES (?1, ?2, ?1, 'p')",
                [id, agent],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('pi:b', 'k', 9, 'prompt', 'fix the watcher')",
            [],
        )
        .unwrap();
        c
    }

    fn feed_keys(c: &rusqlite::Connection, q: &[(&str, &str)]) -> Vec<String> {
        let q: HashMap<String, String> = q
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        super::feed(c, &q).unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| format!("{}{}", &i["itemType"].as_str().unwrap()[..1], i["id"]))
            .collect()
    }

    #[test]
    fn feed_filters_by_type_agent_and_view() {
        let c = fixture();
        assert_eq!(feed_keys(&c, &[]), ["p1", "o5", "o4", "s3", "o2", "o1"]);
        // Types: observation types, summaries; prompts only when asked for or unfiltered.
        assert_eq!(feed_keys(&c, &[("type", "bugfix")]), ["o4", "o1"]);
        assert_eq!(feed_keys(&c, &[("type", "summary,decision")]), ["s3", "o2"]);
        assert_eq!(feed_keys(&c, &[("type", "prompt")]), ["p1"]);
        assert!(feed_keys(&c, &[("type", "no-such-type")]).is_empty());
        // Agent narrows memories and prompts alike.
        assert_eq!(feed_keys(&c, &[("agent", "pi")]), ["p1", "o2"]);
        assert_eq!(feed_keys(&c, &[("agent", "claude")]), ["s3", "o1"]);
        // An agent that is not a plain word filters nothing rather than reaching SQL.
        assert_eq!(feed_keys(&c, &[("agent", "%")]).len(), 6);
        // Views.
        assert_eq!(feed_keys(&c, &[("view", "pinned")]), ["o5"]);
        assert_eq!(feed_keys(&c, &[("view", "edited")]), ["o2"]);
        assert!(feed_keys(&c, &[("view", "unopened")]).is_empty());
        crate::uptake::offered(&c, "claude:a", &[1, 4], "prompt").unwrap();
        crate::uptake::mcp_call(&c, "get_observations", Some("p"), &[4]).unwrap();
        assert_eq!(feed_keys(&c, &[("view", "unopened")]), ["o1"]);
        // Offers to scripted sessions do not count.
        crate::uptake::offered(&c, "pi:b", &[2], "prompt").unwrap();
        crate::scripted::mark(&c, "pi:b").unwrap();
        assert_eq!(feed_keys(&c, &[("view", "unopened")]), ["o1"]);
        // Filters apply to searches too.
        assert_eq!(
            feed_keys(&c, &[("q", "watcher"), ("agent", "codex")]),
            ["o4"]
        );
    }

    fn raw_feed(c: &rusqlite::Connection, q: &[(&str, String)]) -> Value {
        let q: HashMap<String, String> =
            q.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        super::feed(c, &q).unwrap()
    }

    fn page(c: &rusqlite::Connection, q: &[(&str, String)]) -> (Vec<String>, Value) {
        let v = raw_feed(c, q);
        let keys = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| format!("{}{}", &i["itemType"].as_str().unwrap()[..1], i["id"]))
            .collect();
        (keys, v)
    }

    /// Every item once, whatever the page size, with ties in time inside and across
    /// pages and memories and prompts at the same millisecond.
    #[test]
    fn paging_visits_every_item_once_in_both_directions() {
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
        let mut all = Vec::new();
        // Times with many items each: 5 memories and 2 prompts at 100, 3 memories at 200,
        // 1 prompt at 150, 4 memories and 3 prompts at 300.
        let mut id = 0;
        let mut ev = 0;
        for (at, memories, prompts) in [(100, 5, 2), (150, 0, 1), (200, 3, 0), (300, 4, 3)] {
            for _ in 0..memories {
                id += 1;
                c.execute(
                    "INSERT INTO memories(id, project, kind, type, title, origin, origin_id, created_at)
                     VALUES (?1, 'p', 'observation', 'change', 't', 'mnem', ?1, ?2)",
                    [id, at],
                )
                .unwrap();
                all.push(format!("o{id}"));
            }
            for _ in 0..prompts {
                ev += 1;
                c.execute(
                    "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('pi:s', ?1, ?2, 'prompt', 'x')",
                    rusqlite::params![format!("k{ev}"), at],
                )
                .unwrap();
                let eid: i64 = c.last_insert_rowid();
                all.push(format!("p{eid}"));
            }
        }
        all.sort();
        for limit in 1..=6 {
            // Backwards from the newest.
            let mut seen = Vec::new();
            let mut before: Option<String> = None;
            for _ in 0..50 {
                let mut q = vec![("limit", limit.to_string())];
                if let Some(b) = &before {
                    q.push(("before", b.clone()));
                }
                let (keys, v) = page(&c, &q);
                seen.extend(keys);
                match v["next_before"].as_str() {
                    Some(n) => before = Some(n.to_string()),
                    None => break,
                }
            }
            let mut sorted = seen.clone();
            sorted.sort();
            assert_eq!(sorted, all, "backwards, limit {limit}: {seen:?}");

            // Forwards from before the first, as live updates do.
            let mut seen = Vec::new();
            let mut after = "0".to_string();
            for _ in 0..50 {
                let (keys, v) = page(
                    &c,
                    &[("limit", limit.to_string()), ("after", after.clone())],
                );
                seen.extend(keys);
                after = v["newest"].as_str().unwrap().to_string();
                if !v["more"].as_bool().unwrap() {
                    break;
                }
            }
            let mut sorted = seen.clone();
            sorted.sort();
            assert_eq!(sorted, all, "forwards, limit {limit}: {seen:?}");
        }
        // The viewer's start for an empty feed comes before everything, time 0 included.
        c.execute(
            "INSERT INTO memories(id, project, kind, type, title, origin, origin_id, created_at)
             VALUES (99, 'p', 'observation', 'change', 't', 'mnem', 99, NULL)",
            [],
        )
        .unwrap();
        let (keys, _) = page(&c, &[("limit", "1".into()), ("after", "0.-1.0".into())]);
        assert_eq!(keys, ["o99"], "a memory without a time is not skipped");
        c.execute("DELETE FROM memories WHERE id = 99", []).unwrap();
        // Nothing newer: the same cursor comes back, no items.
        let (keys, v) = page(&c, &[("limit", "5".into()), ("after", "300.1.999".into())]);
        assert!(keys.is_empty());
        assert_eq!(v["newest"], "300.1.999");
        // A bare time still works: `before` includes it, `after` excludes it.
        let (keys, _) = page(&c, &[("limit", "50".into()), ("before", "100".into())]);
        assert_eq!(keys.len(), 7);
        let (keys, _) = page(&c, &[("limit", "50".into()), ("after", "200".into())]);
        assert_eq!(keys.len(), 7);
    }

    #[test]
    fn memory_detail_reports_uptake_without_recording_any() {
        let c = fixture();
        crate::uptake::offered(&c, "claude:a", &[1], "start").unwrap();
        crate::uptake::offered(&c, "pi:b", &[1], "prompt").unwrap();
        crate::uptake::mcp_call(&c, "get_observations", Some("p"), &[1, 2]).unwrap();
        let calls = |c: &rusqlite::Connection| -> (i64, i64) {
            c.query_row(
                "SELECT (SELECT count(*) FROM offers), (SELECT count(*) FROM mcp_calls)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let before = calls(&c);
        let d = super::memory_detail(&c, 1).unwrap().unwrap();
        assert_eq!(calls(&c), before, "viewing is not uptake");
        assert_eq!(d["uptake"]["offered"], 2);
        assert_eq!(d["uptake"]["sessions"], 2);
        assert_eq!(d["uptake"]["fetched"], 1);
        assert_eq!(d["session_id"], "claude:a");
        assert_eq!(d["files_now"], serde_json::json!([]));
        assert_eq!(d["files_unchecked"], 0);
        // Id 1 is a substring of id 12's text: only a real fetch of 1 counts.
        crate::uptake::mcp_call(&c, "get_observations", Some("p"), &[12]).unwrap();
        let d = super::memory_detail(&c, 1).unwrap().unwrap();
        assert_eq!(d["uptake"]["fetched"], 1);
        assert!(super::memory_detail(&c, 99).unwrap().is_none());
        let pin = super::memory_detail(&c, 5).unwrap().unwrap();
        assert_eq!(pin["pinned"], true);
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
