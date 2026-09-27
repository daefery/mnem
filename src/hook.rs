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
    /// Verifiable capture status for the injected footer.
    pub fn footer(&self, conn: &Connection) -> String {
        let (sessions, events, last): (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM sessions), (SELECT max(id) FROM events), (SELECT max(ts) FROM events)",
                [],
                |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get(2)?)),
            )
            .unwrap_or((0, 0, None));
        let last = last
            .map(|t| context::ago(db::now_ms() - t))
            .unwrap_or_else(|| "never".into());
        let status = if self.files_behind == 0 {
            "caught up".to_string()
        } else {
            format!(
                "{} transcript(s) still catching up ({:.1} KB), so the last few minutes may be missing",
                self.files_behind,
                self.bytes_behind as f64 / 1e3
            )
        };
        format!(
            "mnem: {sessions} sessions, {events} events indexed · newest event {last} ago · {status}"
        )
    }
}

/// A hook never parses more than this much of one transcript; bigger backlogs are left
/// to `mnem backfill` / `mnem watch` and reported as behind.
const HOOK_MAX_UNREAD: u64 = 8 << 20;

/// Catch up every transcript that changed since it was last read, newest first, until
/// `budget` runs out. This is what makes another agent's work visible in this session.
pub fn catch_up_recent(conn: &mut Connection, budget: Duration) -> Result<Freshness> {
    let deadline = Instant::now() + budget;
    let mut seen: HashMap<String, (u64, i64, Option<i64>)> = HashMap::new();
    {
        let mut s =
            conn.prepare_cached("SELECT path, size_seen, byte_offset, mtime_seen FROM sources")?;
        for r in s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })? {
            let (p, size, off, mtime) = r?;
            seen.insert(p, (size as u64, off, mtime));
        }
    }
    // Size or mtime changed (or never read): ingest decides whether it is an append or
    // a rewrite. Size alone would miss same-length rewrites.
    let mut changed: Vec<(i64, u64, u64, Source)> = ingest::discover()
        .into_iter()
        .filter_map(|s| {
            let m = s.path.metadata().ok()?;
            let mtime = ingest::mtime_ms(&m);
            let (dirty, unread) = match seen.get(s.path.to_string_lossy().as_ref()) {
                Some((size, off, seen_mtime)) => (
                    m.len() != *size || mtime != *seen_mtime,
                    m.len().saturating_sub(*off as u64),
                ),
                None => (true, m.len()),
            };
            dirty.then(|| (mtime.unwrap_or(0), m.len(), unread, s))
        })
        .collect();
    changed.sort_by_key(|c| std::cmp::Reverse(c.0));
    let mut resolver = Resolver::default();
    let mut f = Freshness::default();
    for (_, len, unread, src) in changed {
        if Instant::now() > deadline || unread > HOOK_MAX_UNREAD {
            f.budget_hit |= Instant::now() > deadline;
            f.files_behind += 1;
            f.bytes_behind += unread.min(len);
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
            let mut footer = fresh.footer(conn);
            if let Ok((pending, err)) = crate::distill::pending(conn) {
                if pending > 0 {
                    footer.push_str(&format!(" · {pending} session(s) awaiting distillation"));
                }
                if let Some(e) = err {
                    footer.push_str(&format!(
                        " · last distill error: {}",
                        crate::text::head(&e, 80)
                    ));
                }
            }
            ctx.push_str(&format!("\n---\n{footer}\n"));
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
        // Turn ended: distil it in a detached process so the agent never waits on an LLM.
        "stop" => {
            if let Some(s) = &session {
                spawn_distill(s);
            }
        }
        _ => {}
    }
    Ok(())
}

fn spawn_distill(session: &str) {
    let c = &crate::config::CONFIG.distill;
    if c.on_stop == Some(false) || (c.api_key_env.is_none() && c.api_key_json.is_none()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let args = [
        "distill",
        "--session",
        session,
        "--active",
        "--quiet",
        "--limit",
        "1",
    ];
    // setsid detaches from the agent's process group so the hook returns immediately.
    let spawned = std::process::Command::new("setsid")
        .arg("-f")
        .arg(&exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .or_else(|_| {
            std::process::Command::new(&exe)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        });
    if let Err(e) = spawned {
        log(&format!("spawn distill: {e}"));
    }
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
///
/// Only live work counts: events from transcripts (not imported history), human prompts
/// (not harness), newer than six hours. At most three sessions are shown in full; any
/// others are counted in a "+N more" line, so the watermark can advance past everything
/// without silently dropping anything. Groups are never cut mid-way.
pub fn cross_agent_delta(
    conn: &Connection,
    session: &str,
    project: &str,
) -> Result<Option<String>> {
    const MAX_SESSIONS: usize = 3;
    const MAX_CHARS: usize = 1600;
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
        "SELECT s.id, s.agent, e.kind, e.path, e.text, e.label, e.id FROM events e JOIN sessions s ON s.id = e.session_id
         WHERE e.id > ?1 AND s.project = ?2 AND s.id != ?3 AND e.thread IS NULL
           AND e.source_path IS NOT NULL AND e.ts > ?4
           AND (e.kind IN ('assistant', 'file_edit', 'error') OR (e.kind = 'prompt' AND e.label IS NULL))
         ORDER BY e.id",
    )?;
    struct Other {
        agent: String,
        last_id: i64,
        prompt: Option<String>,
        answer: Option<String>,
        files: Vec<String>,
        error: Option<String>,
    }
    let mut by: Vec<(String, Other)> = Vec::new();
    let mut max_id = wm;
    let mut rows = s.query(params![wm, project, session, db::now_ms() - 6 * 3_600_000])?;
    while let Some(r) = rows.next()? {
        let (sid, agent, kind): (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let (path, text, label, id): (Option<String>, String, Option<String>, i64) =
            (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
        max_id = max_id.max(id);
        let i = match by.iter().position(|(k, _)| *k == sid) {
            Some(i) => i,
            None => {
                by.push((
                    sid,
                    Other {
                        agent,
                        last_id: id,
                        prompt: None,
                        answer: None,
                        files: vec![],
                        error: None,
                    },
                ));
                by.len() - 1
            }
        };
        let o = &mut by[i].1;
        o.last_id = id;
        match kind.as_str() {
            "prompt" => o.prompt = Some(text),
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
    // Everything up to max_id is either shown below or counted in "+N more".
    conn.execute(
        "UPDATE injections SET watermark = ?2 WHERE session_id = ?1",
        params![session, max_id],
    )?;
    // Most recently active first; sessions with an answer or edits carry the most signal.
    by.retain(|(_, o)| o.prompt.is_some() || o.answer.is_some() || !o.files.is_empty());
    if by.is_empty() {
        return Ok(None);
    }
    by.sort_by_key(|(_, o)| {
        std::cmp::Reverse((o.answer.is_some() || !o.files.is_empty(), o.last_id))
    });
    let mut w = String::from("mnem: meanwhile in this project (other sessions, newest first)\n");
    let mut shown = 0;
    for (_, o) in &by {
        if shown == MAX_SESSIONS {
            break;
        }
        let mut g = format!("- {}", o.agent);
        if let Some(p) = &o.prompt {
            g.push_str(&format!(" · asked: {}", context::excerpt(&squash(p), 160)));
        }
        g.push('\n');
        if let Some(a) = &o.answer {
            g.push_str(&format!("  = {}\n", context::excerpt(&squash(a), 280)));
        }
        if !o.files.is_empty() {
            let names: Vec<&str> = o
                .files
                .iter()
                .take(6)
                .map(|p| p.rsplit('/').next().unwrap_or(p))
                .collect();
            g.push_str(&format!("  edited: {}\n", names.join(", ")));
        }
        if let Some(e) = &o.error {
            g.push_str(&format!("  last error: {}\n", crate::text::head(e, 140)));
        }
        if w.len() + g.len() > MAX_CHARS {
            break;
        }
        w.push_str(&g);
        shown += 1;
    }
    if by.len() > shown {
        w.push_str(&format!(
            "+{} more session(s) active here; search memory for details\n",
            by.len() - shown
        ));
    }
    Ok(Some(w))
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
