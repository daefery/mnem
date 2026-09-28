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
    /// The user's prompt (UserPromptSubmit).
    pub prompt: Option<String>,
    /// The file a tool read or changed (PostToolUse on Read, Edit, Write, ...).
    pub file: Option<String>,
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
            prompt: s("prompt"),
            file: ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|k| v["tool_input"].get(k).and_then(Value::as_str))
                .map(str::to_string),
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
    let (sources, cut_short) = ingest::discover_until(Some(deadline));
    let mut changed: Vec<(i64, u64, u64, Source)> = sources
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
    let mut f = Freshness {
        budget_hit: cut_short,
        files_behind: cut_short as usize,
        ..Default::default()
    };
    let mut busy = false;
    for (_, len, unread, src) in changed {
        // After one lock timeout, further writes would wait again; leave them to later.
        if busy || Instant::now() > deadline || unread > HOOK_MAX_UNREAD {
            f.budget_hit |= Instant::now() > deadline;
            f.files_behind += 1;
            f.bytes_behind += unread.min(len);
            continue;
        }
        match ingest::ingest_file(conn, &src, &mut resolver) {
            Ok(o) if o.status == Status::CaughtUp => {}
            Ok(_) => f.files_behind += 1,
            Err(e) => {
                busy |= is_busy(&e);
                log(&format!("catch-up {}: {e:#}", src.path.display()));
                f.files_behind += 1;
            }
        }
    }
    Ok(f)
}

/// Bytes of this transcript not yet ingested.
fn own_unread(conn: &Connection, path: &std::path::Path) -> u64 {
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    let off: i64 = conn
        .query_row(
            "SELECT byte_offset FROM sources WHERE path = ?1",
            params![path.to_string_lossy()],
            |r| r.get(0),
        )
        .unwrap_or(0);
    size.saturating_sub(off as u64)
}

fn is_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>().is_some_and(|r| {
            matches!(
                r.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
        })
    })
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
    // A file touch comes after every read or edit: it stays quick and leaves catching up
    // with the transcript to the other hooks and the watcher.
    if event == "file" {
        if let (Some(session), Some(path), Some(cwd)) = (&session, &input.file, &input.cwd)
            && let Some(t) = crate::files::resolve(path, std::path::Path::new(cwd))
            && let Some(text) = crate::files::on_touch(conn, session, &t, crate::eval::FILE_TOP)?
        {
            emit("PostToolUse", &text);
        }
        return Ok(());
    }
    if let Some(tp) = &input.transcript_path
        && tp.exists()
        && own_unread(conn, tp) <= HOOK_MAX_UNREAD
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
            // Nothing in catch-up may keep the context from being emitted.
            let fresh = catch_up_recent(conn, Duration::from_millis(400)).unwrap_or_else(|e| {
                log(&format!("catch-up: {e:#}"));
                Freshness {
                    files_behind: 1,
                    ..Default::default()
                }
            });
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
            if let Ok((pending, _)) = crate::distill::pending(conn)
                && pending > 0
            {
                footer.push_str(&format!(" · {pending} session(s) awaiting distillation"));
            }
            // Problems go to the user directly (systemMessage), not only to the agent.
            let alerts = crate::health::for_hook(conn);
            for a in &alerts {
                footer.push_str(&format!("\nmnem warning: {a}"));
            }
            ctx.push_str(&format!("\n---\n{footer}\n"));
            let notice = (!alerts.is_empty()).then(|| format!("mnem: {}", alerts.join(" | ")));
            emit_with("SessionStart", &ctx, notice.as_deref());
            if let Some(s) = &session
                && let Err(e) = set_watermark(conn, s)
            {
                log(&format!("watermark: {e:#}"));
            }
        }
        "prompt" => {
            let Some(s) = &session else { return Ok(()) };
            if let Err(e) = catch_up_recent(conn, Duration::from_millis(200)) {
                log(&format!("catch-up: {e:#}"));
            }
            let Some(project) = project_for(conn, Some(s), input.cwd.as_deref()) else {
                return Ok(());
            };
            if let Some(update) = prompt_update(conn, s, &project, input.prompt.as_deref()) {
                emit("UserPromptSubmit", &update);
            }
        }
        // Turn ended: distil it in a detached process so the agent never waits on an LLM.
        "stop" => {
            if let Some(s) = &session {
                let cwd = input.cwd.clone().or_else(|| {
                    conn.query_row("SELECT cwd FROM sessions WHERE id = ?1", params![s], |r| {
                        r.get(0)
                    })
                    .ok()
                    .flatten()
                });
                // git can be slow on big or network repos; never make the agent wait for it.
                if let Some(cwd) = cwd {
                    spawn_detached(&["snapshot", "--session", s, "--cwd", &cwd]);
                }
                spawn_distill(s);
            }
        }
        _ => {}
    }
    Ok(())
}

/// What to add to a prompt: other sessions' news, then memories matching the prompt.
/// Either part failing only drops that part.
pub fn prompt_update(
    conn: &Connection,
    session: &str,
    project: &str,
    prompt: Option<&str>,
) -> Option<String> {
    let delta = cross_agent_delta(conn, session, project).unwrap_or_else(|e| {
        log(&format!("delta: {e:#}"));
        None
    });
    let recalled = prompt.and_then(|p| {
        crate::recall::recall(conn, session, project, p).unwrap_or_else(|e| {
            log(&format!("recall: {e:#}"));
            None
        })
    });
    let parts: Vec<String> = [delta, recalled].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn spawn_distill(session: &str) {
    let c = &crate::config::CONFIG.distill;
    if c.on_stop == Some(false) || (c.api_key_env.is_none() && c.api_key_json.is_none()) {
        return;
    }
    spawn_detached(&[
        "distill",
        "--session",
        session,
        "--active",
        "--quiet",
        "--limit",
        "1",
    ]);
}

/// Run this binary with `args` in the background, detached from the agent's process
/// group (setsid), so the hook returns immediately.
fn spawn_detached(args: &[&str]) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let quiet = |c: &mut std::process::Command| {
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    };
    let mut detached = std::process::Command::new("setsid");
    detached.arg("-f").arg(&exe).args(args);
    quiet(&mut detached);
    let spawned = detached.spawn().or_else(|_| {
        let mut plain = std::process::Command::new(&exe);
        plain.args(args);
        quiet(&mut plain);
        plain.spawn()
    });
    if let Err(e) = spawned {
        log(&format!("spawn {}: {e}", args.first().unwrap_or(&"")));
    }
}

fn emit(event: &str, ctx: &str) {
    emit_with(event, ctx, None);
}

/// `system_message` is shown to the user by Claude Code and Codex.
fn emit_with(event: &str, ctx: &str, system_message: Option<&str>) {
    let mut out =
        json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": ctx } });
    if let Some(m) = system_message {
        out["systemMessage"] = json!(m);
    }
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
/// (not harness), newer than six hours and newer than this session's start. Each other
/// session has its own "seen through" mark, advanced only when that session is shown,
/// so sessions beyond the per-prompt cap stay pending and appear on a later prompt.
pub fn cross_agent_delta(
    conn: &Connection,
    session: &str,
    project: &str,
) -> Result<Option<String>> {
    const MAX_SESSIONS: usize = 3;
    const MAX_CHARS: usize = 1600;
    let floor: Option<i64> = conn
        .query_row(
            "SELECT watermark FROM injections WHERE session_id = ?1",
            params![session],
            |r| r.get(0),
        )
        .optional()?;
    let Some(floor) = floor else {
        set_watermark(conn, session)?;
        return Ok(None);
    };
    let mut s = conn.prepare(
        "SELECT s.id, s.agent, e.kind, e.path, coalesce(e.text, ''), e.label, e.id
         FROM events e JOIN sessions s ON s.id = e.session_id
         LEFT JOIN delta_seen d ON d.viewer = ?3 AND d.other = s.id
         WHERE e.id > max(?1, coalesce(d.through, 0)) AND s.project = ?2 AND s.id != ?3
           AND e.thread IS NULL AND e.source_path IS NOT NULL AND e.ts > ?4
           AND (e.kind IN ('assistant', 'file_edit', 'error') OR (e.kind = 'prompt' AND e.label IS NULL))
         ORDER BY e.id",
    )?;
    struct Other {
        id: String,
        agent: String,
        last_id: i64,
        prompt: Option<String>,
        answer: Option<String>,
        files: Vec<String>,
        error: Option<String>,
    }
    let mut by: Vec<Other> = Vec::new();
    let mut rows = s.query(params![
        floor,
        project,
        session,
        db::now_ms() - 6 * 3_600_000
    ])?;
    while let Some(r) = rows.next()? {
        let (sid, agent, kind): (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let (path, text, label, id): (Option<String>, String, Option<String>, i64) =
            (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
        let i = match by.iter().position(|o| o.id == sid) {
            Some(i) => i,
            None => {
                by.push(Other {
                    id: sid,
                    agent,
                    last_id: id,
                    prompt: None,
                    answer: None,
                    files: vec![],
                    error: None,
                });
                by.len() - 1
            }
        };
        let o = &mut by[i];
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
    if by.is_empty() {
        return Ok(None);
    }
    // Most recently active first; sessions that produced something beat pure chatter.
    by.sort_by_key(|o| {
        std::cmp::Reverse((
            o.answer.is_some() || !o.files.is_empty() || o.error.is_some(),
            o.last_id,
        ))
    });
    let mut w = String::from("mnem: meanwhile in this project (other sessions, newest first)\n");
    let mut shown: Vec<&Other> = Vec::new();
    for o in &by {
        if shown.len() == MAX_SESSIONS {
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
        // Every field is capped so a group always fits the budget whole: a group is
        // either shown completely or left pending, never cut and then marked delivered.
        if let Some(e) = &o.error {
            g.push_str(&format!("  last error: {}\n", crate::text::head(e, 140)));
        }
        if !o.files.is_empty() {
            let names: Vec<String> = o
                .files
                .iter()
                .take(6)
                .map(|p| crate::text::head(p.rsplit('/').next().unwrap_or(p), 40))
                .collect();
            g.push_str(&format!("  edited: {}\n", names.join(", ")));
        }
        if !shown.is_empty() && w.len() + g.len() > MAX_CHARS {
            break;
        }
        w.push_str(&g);
        shown.push(o);
    }
    let mut mark = conn.prepare_cached(
        "INSERT INTO delta_seen(viewer, other, through) VALUES (?1, ?2, ?3)
         ON CONFLICT(viewer, other) DO UPDATE SET through = max(through, excluded.through)",
    )?;
    for o in &shown {
        mark.execute(params![session, o.id, o.last_id])?;
    }
    if by.len() > shown.len() {
        w.push_str(&format!(
            "+{} more session(s) with new work; shown on your next prompts\n",
            by.len() - shown.len()
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
