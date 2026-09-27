//! Import an existing claude-mem database.
//!
//! The source is never opened for writing: it is first copied with `VACUUM INTO` (a
//! consistent snapshot even while claude-mem's worker is still writing), and the import
//! reads that copy. Re-running is safe; rows dedupe on (origin, origin_id).

use crate::adapters;
use crate::db;
use crate::text;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const SUMMARY_ID_BASE: i64 = 1_000_000;

#[derive(Default, Debug)]
pub struct Stats {
    pub sessions_seen: usize,
    pub sessions_added: usize,
    pub observations: usize,
    pub summaries: usize,
    pub prompts: usize,
    pub prompts_skipped: usize,
    pub projects_mapped: usize,
}

struct CmSession {
    mnem_id: String,
    agent: String,
    native: String,
    project: String,
    started: Option<i64>,
    completed: Option<i64>,
    title: Option<String>,
}

/// claude-mem platform names -> mnem agent names.
fn agent_of(platform: &str) -> &str {
    match platform {
        "claude" | "claude-code" => "claude",
        other => other,
    }
}

pub fn snapshot(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    let c = Connection::open_with_flags(
        src,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", src.display()))?;
    c.busy_timeout(std::time::Duration::from_secs(30))?;
    c.execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
    Ok(())
}

pub fn claude_mem(conn: &mut Connection, src: &Path) -> Result<Stats> {
    let snap = db::data_dir().join("claude-mem.snapshot.db");
    snapshot(src, &snap)?;
    let cm = Connection::open_with_flags(&snap, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stats = Stats::default();

    // claude-mem session -> mnem session. memory_session_id is what observations reference.
    let mut all: Vec<CmSession> = Vec::new();
    let mut by_memory: HashMap<String, String> = HashMap::new();
    let mut by_content: HashMap<String, String> = HashMap::new();
    {
        let mut s = cm.prepare(
            "SELECT memory_session_id, content_session_id, platform_source, project,
                    started_at_epoch, completed_at_epoch, custom_title
             FROM sdk_sessions",
        )?;
        let rows = s.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        for row in rows {
            let (memory, content, platform, project, started, completed, title) = row?;
            let agent = agent_of(&platform).to_string();
            let mnem_id = format!("{agent}:{content}");
            by_content.insert(content.clone(), mnem_id.clone());
            stats.sessions_seen += 1;
            if let Some(m) = memory {
                by_memory.insert(m, mnem_id.clone());
            }
            all.push(CmSession {
                mnem_id,
                agent,
                native: content,
                project,
                started,
                completed,
                title,
            });
        }
    }

    let project_map = map_projects(conn, &all)?;
    stats.projects_mapped = project_map.len();
    let proj = |name: &str| {
        project_map
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    };

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut exists = tx.prepare_cached("SELECT 1 FROM sessions WHERE id = ?1")?;
        let mut add = tx.prepare_cached(
            "INSERT INTO sessions(id, agent, native_id, project, title, started_at, last_event_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for s in &all {
            if exists
                .query_row(params![s.mnem_id], |_| Ok(()))
                .optional()?
                .is_none()
            {
                add.execute(params![
                    s.mnem_id,
                    s.agent,
                    s.native,
                    proj(&s.project),
                    s.title,
                    s.started,
                    s.completed.or(s.started)
                ])?;
                stats.sessions_added += 1;
            }
        }
    }

    let mut ins = tx.prepare_cached(
        "INSERT OR IGNORE INTO memories(id, session_id, project, kind, type, title, subtitle, narrative,
            facts, concepts, files_read, files_modified, data, origin, origin_id, model, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'claude-mem', ?14, ?15, ?16)",
    )?;
    {
        let mut s = cm.prepare(
            "SELECT id, memory_session_id, project, type, title, subtitle, narrative, facts, concepts,
                    files_read, files_modified, generated_by_model, created_at_epoch
             FROM observations ORDER BY id",
        )?;
        let mut rows = s.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            anyhow::ensure!(
                id < SUMMARY_ID_BASE,
                "observation id {id} exceeds reserved range"
            );
            let memory: String = r.get(1)?;
            let project: String = r.get(2)?;
            let red = |i: usize| -> rusqlite::Result<Option<String>> {
                Ok(r.get::<_, Option<String>>(i)?.map(|s| text::redact(&s)))
            };
            stats.observations += ins.execute(params![
                id,
                by_memory.get(&memory).cloned(),
                proj(&project),
                "observation",
                r.get::<_, Option<String>>(3)?,
                red(4)?,
                red(5)?,
                red(6)?,
                red(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
                None::<String>,
                format!("obs:{id}"),
                r.get::<_, Option<String>>(11)?,
                r.get::<_, i64>(12)?,
            ])?;
        }
    }
    {
        let mut s = cm.prepare(
            "SELECT id, memory_session_id, project, request, investigated, learned, completed,
                    next_steps, notes, files_read, files_edited, created_at_epoch
             FROM session_summaries ORDER BY id",
        )?;
        let mut rows = s.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let memory: String = r.get(1)?;
            let project: String = r.get(2)?;
            let field = |i: usize| -> rusqlite::Result<String> {
                Ok(text::redact(
                    r.get::<_, Option<String>>(i)?.unwrap_or_default().trim(),
                ))
            };
            let (request, investigated, learned, completed, next, notes) = (
                field(3)?,
                field(4)?,
                field(5)?,
                field(6)?,
                field(7)?,
                field(8)?,
            );
            let narrative = [
                ("Investigated", &investigated),
                ("Learned", &learned),
                ("Completed", &completed),
                ("Next steps", &next),
                ("Notes", &notes),
            ]
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
            let data = serde_json::json!({
                "request": request, "investigated": investigated, "learned": learned,
                "completed": completed, "next_steps": next, "notes": notes,
            });
            // The reserved id may already belong to a memory mnem distilled after an
            // earlier import; then take a fresh id. `sum:<id>` still identifies it.
            let wanted = SUMMARY_ID_BASE + id;
            let taken = tx
                .query_row(
                    "SELECT 1 FROM memories WHERE id = ?1 AND NOT (origin = 'claude-mem' AND origin_id = ?2)",
                    params![wanted, format!("sum:{id}")],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            stats.summaries += ins.execute(params![
                (!taken).then_some(wanted),
                by_memory.get(&memory).cloned(),
                proj(&project),
                "summary",
                None::<String>,
                request,
                None::<String>,
                narrative,
                None::<String>,
                None::<String>,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
                data.to_string(),
                format!("sum:{id}"),
                None::<String>,
                r.get::<_, i64>(11)?,
            ])?;
        }
    }
    drop(ins);

    // Prompts only for sessions whose transcript is gone; live transcripts are richer.
    {
        let mut has_prompts: HashSet<String> = HashSet::new();
        let mut q = tx.prepare("SELECT DISTINCT session_id FROM events WHERE kind = 'prompt'")?;
        for r in q.query_map([], |r| r.get::<_, String>(0))? {
            has_prompts.insert(r?);
        }
        let mut add = tx.prepare_cached(
            "INSERT OR IGNORE INTO events(session_id, record_key, ts, turn, kind, text, label)
             VALUES (?1, ?2, ?3, ?4, 'prompt', ?5, ?6)",
        )?;
        let mut s = cm.prepare(
            "SELECT id, content_session_id, prompt_number, prompt_text, created_at_epoch
             FROM user_prompts ORDER BY content_session_id, prompt_number, id",
        )?;
        let mut rows = s.query([])?;
        let mut last: Option<(String, String)> = None;
        while let Some(r) = rows.next()? {
            let content: String = r.get(1)?;
            let Some(sid) = by_content.get(&content) else {
                continue;
            };
            if has_prompts.contains(sid) {
                stats.prompts_skipped += 1;
                continue;
            }
            // Same filter as live transcripts: drop harness wrappers, label injected
            // prompts, collapse consecutive repeats.
            let t: String = r.get(3)?;
            let Some((t, label)) = adapters::classify_prompt(&t) else {
                continue;
            };
            let key = (sid.clone(), text::hash(&t));
            if last.as_ref() == Some(&key) {
                continue;
            }
            last = Some(key);
            let id: i64 = r.get(0)?;
            stats.prompts += add.execute(params![
                sid,
                format!("cm:prompt:{id}"),
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(2)?,
                text::clean(&t, 4000),
                label,
            ])?;
        }
    }
    // Re-point rows written by earlier imports when the mapping has since improved.
    for (name, p) in &project_map {
        tx.execute(
            "UPDATE memories SET project = ?2 WHERE origin = 'claude-mem' AND project = ?1",
            params![name, p],
        )?;
        tx.execute(
            "UPDATE sessions SET project = ?2 WHERE project = ?1 AND NOT EXISTS (SELECT 1 FROM sources WHERE session_id = sessions.id)",
            params![name, p],
        )?;
    }
    tx.execute(
        "INSERT INTO meta(k, v) VALUES ('import.claude-mem', ?1)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![
            serde_json::json!({ "at": db::now_ms(), "source": src.to_string_lossy() }).to_string()
        ],
    )?;
    tx.commit()?;
    let _ = std::fs::remove_file(&snap);
    Ok(stats)
}

/// claude-mem names projects by folder basename ("firstmate", but also "code" or a home
/// directory). A name maps to a mnem project (git identity) only when the repo name
/// matches it; among several such repos, the one most often seen for sessions both
/// systems know wins. Folder names like "code" stay as they are.
fn map_projects(conn: &Connection, sessions: &[CmSession]) -> Result<HashMap<String, String>> {
    // "github.com/o/repo" -> "repo"; "github.com/o/repo#checkout" -> "checkout".
    let tail = |p: &str| {
        let p = p.trim_end_matches('/');
        match p.rsplit_once('#') {
            Some((_, checkout)) => checkout.to_lowercase(),
            None => p.rsplit('/').next().unwrap_or(p).to_lowercase(),
        }
    };
    let mut votes: HashMap<(String, String), usize> = HashMap::new();
    // Only transcript-backed sessions vote; sessions created by a previous import carry
    // the raw claude-mem name.
    let mut q = conn.prepare_cached(
        "SELECT s.project FROM sessions s
         WHERE s.id = ?1 AND s.project IS NOT NULL AND EXISTS (SELECT 1 FROM sources WHERE session_id = s.id)",
    )?;
    for s in sessions {
        if let Some(p) = q
            .query_row(params![s.mnem_id], |r| r.get::<_, String>(0))
            .optional()?
        {
            *votes.entry((s.project.clone(), p)).or_default() += 1;
        }
    }
    let mut q = conn.prepare(
        "SELECT DISTINCT project FROM sessions s
         WHERE project IS NOT NULL AND project NOT LIKE '/%'
           AND EXISTS (SELECT 1 FROM sources WHERE session_id = s.id)",
    )?;
    let repos: Vec<String> = q
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut map = HashMap::new();
    for s in sessions {
        if map.contains_key(&s.project) {
            continue;
        }
        let name = tail(&s.project);
        let best = repos.iter().filter(|r| tail(r) == name).max_by_key(|r| {
            votes
                .get(&(s.project.clone(), (*r).clone()))
                .copied()
                .unwrap_or(0)
        });
        if let Some(r) = best {
            map.insert(s.project.clone(), r.clone());
        }
    }
    Ok(map)
}
