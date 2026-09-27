//! Agent hook entry point: `mnem hook <agent> <event>` with the hook JSON on stdin.
//!
//! Claude Code and Codex share the protocol: stdin carries `session_id`,
//! `transcript_path`, `cwd`; stdout `{"hookSpecificOutput": {"hookEventName",
//! "additionalContext"}}` adds model context on SessionStart and UserPromptSubmit.
//!
//! A hook never blocks the agent on failure: errors are logged and it exits 0.

use crate::context;
use crate::db;
use crate::ingest::{self, Source, Status};
use crate::model::Agent;
use crate::project::Resolver;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Input {
    pub session_id: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub cwd: Option<String>,
}

impl Input {
    pub fn from_stdin() -> Input {
        let mut buf = String::new();
        let _ = std::io::stdin().take(16 << 20).read_to_string(&mut buf);
        let v: Value = serde_json::from_str(&buf).unwrap_or(Value::Null);
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Input {
            session_id: s("session_id"),
            transcript_path: s("transcript_path").map(PathBuf::from),
            cwd: s("cwd"),
        }
    }
}

#[derive(Default, Debug)]
pub struct Freshness {
    pub files_behind: usize,
    pub bytes_behind: u64,
    pub budget_hit: bool,
}

impl Freshness {
    pub fn footer(&self) -> String {
        if self.files_behind == 0 {
            "mnem: capture caught up across Claude Code, Codex and pi".into()
        } else {
            format!(
                "mnem: {} transcript(s) still catching up ({:.1} KB); context may miss the last few minutes",
                self.files_behind,
                self.bytes_behind as f64 / 1e3
            )
        }
    }
}

/// Catch up every transcript that changed since its cursor, newest first, within
/// `budget`. This is what makes another agent's work visible in this session.
pub fn catch_up_recent(conn: &mut Connection, budget: Duration) -> Result<Freshness> {
    let t0 = Instant::now();
    let mut seen: HashMap<String, (u64, i64)> = HashMap::new();
    {
        let mut s = conn.prepare_cached("SELECT path, size_seen, byte_offset FROM sources")?;
        for r in s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (p, size, off) = r?;
            seen.insert(p, (size as u64, off));
        }
    }
    let mut changed: Vec<(std::time::SystemTime, u64, Source)> = ingest::discover()
        .into_iter()
        .filter_map(|s| {
            let m = s.path.metadata().ok()?;
            let known = seen.get(s.path.to_string_lossy().as_ref());
            let dirty = match known {
                Some((size, off)) => m.len() != *size || m.len() as i64 != *off,
                None => true,
            };
            dirty.then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), s))
        })
        .collect();
    changed.sort_by_key(|c| std::cmp::Reverse(c.0));
    let mut resolver = Resolver::default();
    let mut f = Freshness::default();
    for (_, len, src) in changed {
        if t0.elapsed() > budget {
            f.budget_hit = true;
            f.files_behind += 1;
            f.bytes_behind += len;
            continue;
        }
        match ingest::ingest_file(conn, &src, &mut resolver) {
            Ok(o) if o.status == Status::CaughtUp => {}
            Ok(_) => f.files_behind += 1,
            Err(e) => {
                log(&format!("catch-up {}: {e:#}", src.path.display()));
                f.files_behind += 1;
            }
        }
    }
    Ok(f)
}

pub fn session_key(agent: Agent, native: &str) -> String {
    format!("{}:{native}", agent.as_str())
}

pub fn project_for(conn: &Connection, session: Option<&str>, cwd: Option<&str>) -> Option<String> {
    if let Some(s) = session
        && let Ok(Some(p)) = conn
            .query_row(
                "SELECT project FROM sessions WHERE id = ?1",
                params![s],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
    {
        return Some(p);
    }
    Resolver::default().resolve(cwd, None)
}

pub fn run(conn: &mut Connection, agent: Agent, event: &str) -> Result<()> {
    let input = Input::from_stdin();
    let session = input.session_id.as_deref().map(|s| session_key(agent, s));
    if let Some(tp) = &input.transcript_path
        && tp.exists()
    {
        let mut r = Resolver::default();
        if let Err(e) = ingest::ingest_file(
            conn,
            &Source {
                path: tp.clone(),
                agent,
            },
            &mut r,
        ) {
            log(&format!("ingest {}: {e:#}", tp.display()));
        }
    }
    match event {
        "session-start" => {
            let fresh = catch_up_recent(conn, Duration::from_millis(400))?;
            let Some(project) = project_for(conn, session.as_deref(), input.cwd.as_deref()) else {
                return Ok(());
            };
            let mut ctx = context::build(
                conn,
                &context::Options {
                    project: &project,
                    current: session.as_deref(),
                    budget_chars: 8000,
                    sessions: 5,
                    turns: 3,
                    observations: 30,
                },
            )?;
            ctx.push_str(&format!("\n---\n{}\n", fresh.footer()));
            if let Some(s) = &session {
                set_watermark(conn, s)?;
            }
            emit("SessionStart", &ctx);
        }
        "prompt" => {
            let Some(s) = &session else { return Ok(()) };
            catch_up_recent(conn, Duration::from_millis(200))?;
            let Some(project) = project_for(conn, Some(s), input.cwd.as_deref()) else {
                return Ok(());
            };
            if let Some(delta) = cross_agent_delta(conn, s, &project)? {
                emit("UserPromptSubmit", &delta);
            }
        }
        // Stop / SessionEnd: the transcript catch-up above is the whole job for now.
        _ => {}
    }
    Ok(())
}

fn emit(event: &str, ctx: &str) {
    let out = json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": ctx } });
    println!("{out}");
}

fn set_watermark(conn: &Connection, session: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO injections(session_id, watermark) VALUES (?1, (SELECT coalesce(max(id), 0) FROM events))
         ON CONFLICT(session_id) DO UPDATE SET watermark = excluded.watermark",
        params![session],
    )?;
    Ok(())
}

/// What other sessions in this project did since this session last looked.
/// Empty (None) most of the time; capped so it never crowds out the prompt.
pub fn cross_agent_delta(
    conn: &Connection,
    session: &str,
    project: &str,
) -> Result<Option<String>> {
    let wm: Option<i64> = conn
        .query_row(
            "SELECT watermark FROM injections WHERE session_id = ?1",
            params![session],
            |r| r.get(0),
        )
        .optional()?;
    let Some(wm) = wm else {
        set_watermark(conn, session)?;
        return Ok(None);
    };
    let mut s = conn.prepare(
        "SELECT s.id, s.agent, e.kind, e.label, e.path, e.text, e.id FROM events e JOIN sessions s ON s.id = e.session_id
         WHERE e.id > ?1 AND s.project = ?2 AND s.id != ?3 AND e.thread IS NULL
           AND e.kind IN ('prompt', 'assistant', 'file_edit', 'error')
         ORDER BY e.id",
    )?;
    struct Other {
        agent: String,
        prompt: Option<String>,
        answer: Option<String>,
        files: Vec<String>,
        error: Option<String>,
    }
    let mut by: Vec<(String, Other)> = Vec::new();
    let mut max_id = wm;
    let mut rows = s.query(params![wm, project, session])?;
    while let Some(r) = rows.next()? {
        let (sid, agent, kind): (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let (label, path, text, id): (Option<String>, Option<String>, String, i64) =
            (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
        max_id = max_id.max(id);
        let o = match by.iter_mut().find(|(k, _)| *k == sid) {
            Some((_, o)) => o,
            None => {
                by.push((
                    sid,
                    Other {
                        agent,
                        prompt: None,
                        answer: None,
                        files: vec![],
                        error: None,
                    },
                ));
                &mut by.last_mut().expect("just pushed").1
            }
        };
        match kind.as_str() {
            "prompt" if label.is_none() => o.prompt = Some(text),
            "assistant" => o.answer = Some(text),
            "file_edit" => {
                if let Some(p) = path
                    && !o.files.contains(&p)
                {
                    o.files.push(p);
                }
            }
            "error" => {
                o.error = Some(format!(
                    "{}: {}",
                    label.unwrap_or_default(),
                    text.lines().next().unwrap_or("")
                ))
            }
            _ => {}
        }
    }
    drop(rows);
    conn.execute(
        "UPDATE injections SET watermark = ?2 WHERE session_id = ?1",
        params![session, max_id],
    )?;
    if by.is_empty() {
        return Ok(None);
    }
    let mut w = String::from("mnem: meanwhile in this project (other sessions)\n");
    for (_, o) in &by {
        w.push_str(&format!("- {}", o.agent));
        if let Some(p) = &o.prompt {
            w.push_str(&format!(" · asked: {}", crate::text::head(&squash(p), 140)));
        }
        w.push('\n');
        if let Some(a) = &o.answer {
            w.push_str(&format!("  = {}\n", crate::text::head(&squash(a), 220)));
        }
        if !o.files.is_empty() {
            let names: Vec<&str> = o
                .files
                .iter()
                .take(6)
                .map(|p| p.rsplit('/').next().unwrap_or(p))
                .collect();
            w.push_str(&format!("  edited: {}\n", names.join(", ")));
        }
        if let Some(e) = &o.error {
            w.push_str(&format!("  last error: {}\n", crate::text::head(e, 140)));
        }
    }
    Ok(Some(crate::text::head(&w, 1600)))
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn log(msg: &str) {
    use std::io::Write;
    let path = db::data_dir().join("hook.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{} {msg}", db::now_ms());
    }
}
