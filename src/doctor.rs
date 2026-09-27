//! Capture health. The point of mnem is that silent loss becomes a visible number.

use crate::db;
use crate::ingest;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::{HashMap, HashSet};

/// Print the report. Returns false when any live transcript has unread bytes.
pub fn run(conn: &Connection) -> Result<bool> {
    let present = ingest::discover();
    let mut cursors: HashMap<String, (i64, bool)> = HashMap::new();
    {
        let mut s = conn.prepare("SELECT path, byte_offset, excluded FROM sources")?;
        for r in s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })? {
            let (p, o, x) = r?;
            cursors.insert(p, (o, x));
        }
    }

    println!("transcripts");
    let mut healthy = true;
    for agent in ["claude", "codex", "pi"] {
        let (mut files, mut excluded, mut untracked, mut lagging, mut disk, mut lag) =
            (0, 0, 0, 0, 0u64, 0u64);
        for s in present.iter().filter(|s| s.agent.as_str() == agent) {
            files += 1;
            let size = s.path.metadata().map(|m| m.len()).unwrap_or(0);
            disk += size;
            match cursors.get(s.path.to_string_lossy().as_ref()) {
                Some((_, true)) => excluded += 1,
                Some((off, false)) => {
                    // A file smaller than the cursor was rewritten; all of it must be re-read.
                    let off = *off as u64;
                    let behind = if size < off { size } else { size - off };
                    if behind > 0 {
                        lagging += 1;
                        lag += behind;
                    }
                }
                None => {
                    untracked += 1;
                    lag += size;
                }
            }
        }
        if lag > 0 {
            healthy = false;
        }
        println!(
            "  {agent:<6} {files:>5} files {:>7.1} MB | excluded {excluded} | untracked {untracked} | lagging {lagging} ({:.1} KB behind)",
            disk as f64 / 1e6,
            lag as f64 / 1e3
        );
    }
    let (missing, lost_files, lost_bytes): (i64, i64, i64) = conn.query_row(
        "SELECT count(*),
                coalesce(sum(size_seen > byte_offset), 0),
                coalesce(sum(max(size_seen - byte_offset, 0)), 0)
         FROM sources WHERE missing_since IS NOT NULL AND excluded = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let quarantined: i64 = conn.query_row("SELECT count(*) FROM quarantine", [], |r| r.get(0))?;
    println!("  deleted by agent: {missing} files | quarantined lines: {quarantined}");
    if lost_files > 0 {
        // Permanent: the agent deleted these before mnem read their tail.
        println!(
            "  LOST: {lost_files} files deleted with {:.1} KB never captured",
            lost_bytes as f64 / 1e3
        );
    }

    println!("sessions");
    let mut s = conn.prepare(
        "SELECT s.agent, count(DISTINCT s.id),
                sum(CASE WHEN s.project LIKE '/%' THEN 1 ELSE 0 END),
                max(s.last_event_at)
         FROM sessions s GROUP BY s.agent ORDER BY s.agent",
    )?;
    for r in s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })? {
        let (agent, n, pathonly, last) = r?;
        let ago = last
            .map(|t| fmt_ago(db::now_ms() - t))
            .unwrap_or_else(|| "never".into());
        println!(
            "  {agent:<6} {n:>5} sessions | {pathonly} without git identity | last event {ago} ago"
        );
    }

    println!("events");
    let mut s = conn.prepare("SELECT kind, count(*) FROM events GROUP BY kind ORDER BY 2 DESC")?;
    let kinds: Vec<String> = s
        .query_map([], |r| {
            Ok(format!(
                "{}={}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    println!("  {}", kinds.join(" "));

    match crate::backup::newest_age(&crate::backup::dir()) {
        Some(age) if age <= 2 * crate::backup::INTERVAL_MS => {
            println!(
                "backup: newest verified snapshot {} ago",
                crate::context::ago(age)
            )
        }
        Some(age) => {
            healthy = false;
            println!(
                "backup: STALE, newest verified snapshot {} ago (run `mnem backup`)",
                crate::context::ago(age)
            );
        }
        None => {
            healthy = false;
            println!("backup: NONE, run `mnem backup` (the watch service does this nightly)");
        }
    }
    claude_mem_comparison(conn)?;
    println!(
        "status: {}",
        if healthy {
            "OK, fully caught up"
        } else {
            "BEHIND, run `mnem backfill`"
        }
    );
    Ok(healthy)
}

/// For sessions where claude-mem stored zero observations, does mnem have the work?
fn claude_mem_comparison(conn: &Connection) -> Result<()> {
    let path = db::home().join(".claude-mem/claude-mem.db");
    if !path.exists() {
        return Ok(());
    }
    let cm = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let cutoff = db::now_ms() - 30 * 86_400_000;
    let mut s = cm.prepare(
        "SELECT s.platform_source, s.content_session_id,
                EXISTS(SELECT 1 FROM observations o WHERE o.memory_session_id = s.memory_session_id)
         FROM sdk_sessions s WHERE s.started_at_epoch > ?1",
    )?;
    let rows: Vec<(String, String, bool)> = s
        .query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut have: HashSet<String> = HashSet::new();
    let mut q = conn
        .prepare("SELECT DISTINCT session_id FROM events WHERE kind IN ('prompt', 'assistant')")?;
    for r in q.query_map([], |r| r.get::<_, String>(0))? {
        have.insert(r?);
    }
    println!("claude-mem comparison (last 30 days)");
    for agent in ["claude", "codex", "pi"] {
        let empty: Vec<&(String, String, bool)> = rows
            .iter()
            .filter(|(p, _, has)| p == agent && !has)
            .collect();
        let total = rows.iter().filter(|(p, _, _)| p == agent).count();
        if agent == "pi" {
            // claude-mem-pi mints its own session ids; they do not match pi's transcripts.
            println!(
                "  pi     {total:>4} sessions, {} with 0 observations (ids not mappable)",
                empty.len()
            );
            continue;
        }
        let recovered = empty
            .iter()
            .filter(|(_, id, _)| have.contains(&format!("{agent}:{id}")))
            .count();
        println!(
            "  {agent:<6} {total:>4} sessions, {} with 0 observations -> {recovered} recovered by mnem, {} transcript gone",
            empty.len(),
            empty.len() - recovered
        );
    }
    Ok(())
}

fn fmt_ago(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        ..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}
