//! Merging another person's backup into this memory: add, never change.
//!
//! A replace (`backup::restore`) makes this machine's memory a copy of the backup. A
//! merge keeps every row here as it is and only adds the backup's sessions, events and
//! memories, for learning from a teammate's history. The rules:
//!
//! - Nothing here is updated or deleted. Rows from the backup get new ids here, and
//!   every reference to them (evidence, origin ids, distill marks) is rewritten to match.
//! - A project name the two sides share is renamed on the incoming side to
//!   `<prefix>/<name>`, so their sessions and memories never mix into yours. Names only
//!   they have keep their name.
//! - What is already here is skipped: the same session id (your own copy of a session,
//!   or a teammate backup merged twice), the same memory origin id.
//! - What you forgot stays forgotten: tombstoned sessions, projects, events and memories
//!   are not brought back by a merge.
//! - Incoming sessions are history from another machine: their transcripts are not on
//!   this disk, so they are never re-read, distilled or treated as live work here.
//! - It all happens in one transaction after a safety snapshot of this memory, so a
//!   failure leaves nothing half merged.
//!
//! The backup is checked and sanitised first exactly as for a replace (`backup::stage`).

use crate::db;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// What a merge will do, or did.
#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Plan {
    /// The prefix incoming shared project names get.
    pub prefix: String,
    /// Shared project names and what they become: (name, renamed).
    pub renamed: Vec<(String, String)>,
    /// Incoming projects that keep their name (only the backup has them).
    pub kept: usize,
    pub sessions: usize,
    pub events: usize,
    pub memories: usize,
    pub pins: usize,
    /// Already here (same session or memory), or forgotten here: left out.
    pub skipped_sessions: usize,
    pub skipped_memories: usize,
}

/// Whether this memory holds nothing to keep: then a merge is a plain restore.
pub fn is_empty(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT NOT EXISTS (SELECT 1 FROM sessions) AND NOT EXISTS (SELECT 1 FROM memories)",
        [],
        |r| r.get(0),
    )?)
}

/// A prefix for incoming shared project names: letters, digits, `.`, `_` and `-`,
/// 1 to 40 characters. Default: the backup's machine name.
pub fn check_prefix(prefix: &str) -> Result<String> {
    let p = prefix.trim().trim_matches('/');
    ensure!(
        !p.is_empty()
            && p.len() <= 40
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "the prefix must be 1 to 40 letters, digits, '.', '_' or '-'"
    );
    Ok(p.to_string())
}

/// A default prefix from the backup's machine name (or "team").
pub fn default_prefix(host: Option<&str>) -> String {
    let p: String = host
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(40)
        .collect();
    if p.is_empty() { "team".into() } else { p }
}

/// What merging `snapshot` into `conn` would do. The snapshot is checked and staged the
/// same way as for the merge itself.
pub fn preview(snapshot: &Path, conn: &Connection, prefix: &str) -> Result<Plan> {
    let prefix = check_prefix(prefix)?;
    let (_, staged) = crate::backup::stage(snapshot)?;
    let result = (|| {
        let theirs = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let p = plan(conn, &theirs, &prefix)?;
        Ok(p.summary)
    })();
    crate::backup::remove_db(&staged);
    result
}

/// Merge `snapshot` into `conn`'s database. A verified snapshot of the current memory
/// is taken first (in `backups`); then everything is added in one transaction.
pub fn merge(snapshot: &Path, conn: &mut Connection, backups: &Path, prefix: &str) -> Result<Plan> {
    let prefix = check_prefix(prefix)?;
    let (_, staged) = crate::backup::stage(snapshot)?;
    let result = (|| -> Result<Plan> {
        let theirs = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let keep = crate::backup::create(conn, backups, crate::backup::KEEP + 1)
            .context("could not snapshot the current memory before merging; nothing was changed")?;
        crate::hook::log(&format!("merge: current memory saved as {}", keep.file));
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let p = plan(&tx, &theirs, &prefix)?;
        apply(&tx, &theirs, &p)?;
        tx.commit()?;
        Ok(p.summary)
    })();
    crate::backup::remove_db(&staged);
    result
}

/// An incoming memory as the plan sees it: id, session, project, kind, origin, origin id.
type IncomingMemory = (i64, Option<String>, Option<String>, String, String, String);

/// The full plan: which rows come in, and how names and ids map.
struct Full {
    summary: Plan,
    /// incoming project name -> name here (None stays None).
    projects: HashMap<String, String>,
    /// incoming session ids to add.
    sessions: HashSet<String>,
    /// incoming memory ids to add.
    memories: Vec<i64>,
}

fn projects_of(c: &Connection) -> Result<HashSet<String>> {
    let mut st = c.prepare(
        "SELECT project FROM sessions WHERE project IS NOT NULL
         UNION SELECT project FROM memories WHERE project IS NOT NULL AND project != '*'",
    )?;
    Ok(st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn forgotten(c: &Connection, kind: &str, key: &str) -> Result<bool> {
    Ok(
        c.prepare_cached("SELECT 1 FROM forgotten WHERE kind = ?1 AND key = ?2")?
            .query_row(params![kind, key], |_| Ok(()))
            .optional()?
            .is_some(),
    )
}

/// Where earlier merges with this prefix put each incoming project name, so merging the
/// same teammate again lands every project in the same place (a project that came in
/// unrenamed the first time is "shared" the second time, and must not move).
fn earlier_names(here: &Connection, prefix: &str) -> Result<HashMap<String, String>> {
    let v: Option<String> = here
        .query_row(
            "SELECT v FROM meta WHERE k = ?1",
            [format!("merge.projects.{prefix}")],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}

fn plan(here: &Connection, theirs: &Connection, prefix: &str) -> Result<Full> {
    let mine = projects_of(here)?;
    let incoming = projects_of(theirs)?;
    let earlier = earlier_names(here, prefix)?;
    let mut projects = HashMap::new();
    let mut renamed = Vec::new();
    let mut kept = 0;
    for p in &incoming {
        let to = if let Some(to) = earlier.get(p) {
            to.clone()
        } else if mine.contains(p) {
            prefixed(prefix, p)
        } else {
            p.clone()
        };
        if &to == p {
            kept += 1;
        } else {
            renamed.push((p.clone(), to.clone()));
        }
        projects.insert(p.clone(), to);
    }
    renamed.sort();

    let mut sessions = HashSet::new();
    let mut skipped_sessions = 0;
    {
        let mut st = theirs.prepare("SELECT id, project FROM sessions")?;
        let rows: Vec<(String, Option<String>)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut exists = here.prepare_cached("SELECT 1 FROM sessions WHERE id = ?1")?;
        for (id, project) in rows {
            let target = project.as_ref().map(|p| projects[p].clone());
            let skip = exists.exists([&id])?
                || forgotten(here, "session", &id)?
                || target
                    .as_deref()
                    .is_some_and(|t| forgotten(here, "project", t).unwrap_or(false));
            if skip {
                skipped_sessions += 1;
            } else {
                sessions.insert(id);
            }
        }
    }

    let mut memories = Vec::new();
    let mut skipped_memories = 0;
    let mut pins = 0;
    {
        let mut st = theirs.prepare(
            "SELECT id, session_id, project, kind, origin, origin_id FROM memories ORDER BY id",
        )?;
        let rows: Vec<IncomingMemory> = st
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (id, session, project, kind, origin, origin_id) in rows {
            let target = project.as_deref().map(|p| target_project(&projects, p));
            let origin_id = rewrite_origin_id(&origin, &origin_id, target.as_deref(), None);
            let skip = session.as_ref().is_some_and(|s| !sessions.contains(s))
                || here
                    .prepare_cached("SELECT 1 FROM memories WHERE origin = ?1 AND origin_id = ?2")?
                    .exists(params![origin, origin_id])?
                || forgotten(here, "memory", &format!("{origin}:{origin_id}"))?
                || target
                    .as_deref()
                    .is_some_and(|t| forgotten(here, "project", t).unwrap_or(false));
            if skip {
                skipped_memories += 1;
            } else {
                pins += (kind == "pinned") as usize;
                memories.push(id);
            }
        }
    }

    let events: usize = {
        let mut st =
            theirs.prepare("SELECT session_id, count(*) FROM events GROUP BY session_id")?;
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .filter_map(|r| r.ok())
            .filter(|(s, _)| sessions.contains(s))
            .map(|(_, n)| n as usize)
            .sum()
    };
    Ok(Full {
        summary: Plan {
            prefix: prefix.to_string(),
            renamed,
            kept,
            sessions: sessions.len(),
            events,
            memories: memories.len() - pins,
            pins,
            skipped_sessions,
            skipped_memories,
        },
        projects,
        sessions,
        memories,
    })
}

/// `<prefix>/<name>`; a folder path (`/home/ana/x`) becomes `<prefix>/home/ana/x`.
fn prefixed(prefix: &str, p: &str) -> String {
    format!("{prefix}/{}", p.trim_start_matches('/'))
}

/// Where an incoming memory's project lands: renamed when shared, `*` (global pins)
/// stays global.
fn target_project(projects: &HashMap<String, String>, p: &str) -> String {
    if p == "*" {
        return p.to_string();
    }
    projects.get(p).cloned().unwrap_or_else(|| p.to_string())
}

/// An origin id that names a project or event ids must follow the merge: pins carry
/// their project (`pin:<project>:<hash>`), distilled memories their event range
/// (`<session>@<from>-<through>#<n>`, remapped when `events` is given).
fn rewrite_origin_id(
    origin: &str,
    origin_id: &str,
    project: Option<&str>,
    events: Option<&HashMap<i64, i64>>,
) -> String {
    if origin == "user"
        && let Some(rest) = origin_id.strip_prefix("pin:")
        && let Some((_, hash)) = rest.rsplit_once(':')
        && let Some(p) = project
    {
        return format!("pin:{p}:{hash}");
    }
    if origin == "mnem"
        && let Some(map) = events
        && let Some((sid, tail)) = origin_id.rsplit_once('@')
        && let Some((range, n)) = tail.split_once('#')
        && let Some((a, b)) = range.split_once('-')
        && let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>())
    {
        // The range's ends are event ids of the same session: map each to its new id,
        // or to the nearest mapped id inside the range when an end was not copied.
        let lo = map.get(&a).copied().or_else(|| nearest(map, a, b, true));
        let hi = map.get(&b).copied().or_else(|| nearest(map, a, b, false));
        if let (Some(lo), Some(hi)) = (lo, hi) {
            return format!("{sid}@{lo}-{hi}#{n}");
        }
    }
    origin_id.to_string()
}

fn nearest(map: &HashMap<i64, i64>, a: i64, b: i64, low: bool) -> Option<i64> {
    let inside = map.iter().filter(|(k, _)| **k >= a && **k <= b);
    if low {
        inside.min_by_key(|(k, _)| **k).map(|(_, v)| *v)
    } else {
        inside.max_by_key(|(k, _)| **k).map(|(_, v)| *v)
    }
}

fn apply(here: &Connection, theirs: &Connection, f: &Full) -> Result<()> {
    let proj = |p: Option<String>| p.map(|p| target_project(&f.projects, &p));

    // Sessions.
    {
        let mut st = theirs.prepare(
            "SELECT id, agent, native_id, project, cwd, git_branch, title, started_at, last_event_at FROM sessions",
        )?;
        let mut ins = here.prepare(
            "INSERT INTO sessions(id, agent, native_id, project, cwd, git_branch, title, started_at, last_event_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?;
        for r in rows {
            let (id, agent, native, project, cwd, branch, title, started, last) = r?;
            if !f.sessions.contains(&id) {
                continue;
            }
            ins.execute(params![
                id,
                agent,
                native,
                proj(project),
                cwd,
                branch,
                title.map(|t| crate::text::redact(&t)),
                started,
                last
            ])?;
        }
    }

    // Events, with new ids. Forgotten events stay out. No source path: the transcript
    // is on the other machine, so nothing here reads it again or treats it as live.
    let mut event_ids: HashMap<i64, i64> = HashMap::new();
    {
        let mut st = theirs.prepare(
            "SELECT id, session_id, record_key, ts, turn, kind, tool, path, text, is_error, thread, label, tool_raw
             FROM events ORDER BY id",
        )?;
        let mut ins = here.prepare(
            "INSERT INTO events(session_id, record_key, ts, turn, kind, tool, path, text, is_error, thread, label, tool_raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, bool>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?,
            ))
        })?;
        for r in rows {
            let (id, sid, key, ts, turn, kind, tool, path, text, err, thread, label, raw) = r?;
            if !f.sessions.contains(&sid) || crate::forget::event_forgotten(here, &sid, &key)? {
                continue;
            }
            ins.execute(params![
                sid,
                key,
                ts,
                turn,
                kind,
                tool,
                path,
                text.map(|t| crate::text::redact(&t)),
                err,
                thread,
                label,
                raw
            ])?;
            event_ids.insert(id, here.last_insert_rowid());
        }
    }

    // Memories, with new ids, their origin ids following the projects and event ids.
    let mut memory_ids: HashMap<i64, i64> = HashMap::new();
    {
        let wanted: HashSet<i64> = f.memories.iter().copied().collect();
        let mut st = theirs.prepare(
            "SELECT id, session_id, project, kind, type, title, subtitle, narrative, facts, concepts,
                    files_read, files_modified, data, origin, origin_id, model, created_at
             FROM memories ORDER BY id",
        )?;
        let mut ins = here.prepare(
            "INSERT OR IGNORE INTO memories(session_id, project, kind, type, title, subtitle, narrative, facts, concepts,
                 files_read, files_modified, data, origin, origin_id, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if !wanted.contains(&id) {
                continue;
            }
            let project = proj(r.get(2)?);
            let origin: String = r.get(13)?;
            let origin_id = rewrite_origin_id(
                &origin,
                &r.get::<_, String>(14)?,
                project.as_deref(),
                Some(&event_ids),
            );
            let red = |i: usize| -> rusqlite::Result<Option<String>> {
                Ok(r.get::<_, Option<String>>(i)?
                    .map(|t| crate::text::redact(&t)))
            };
            let n = ins.execute(params![
                r.get::<_, Option<String>>(1)?,
                project,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                red(5)?,
                red(6)?,
                red(7)?,
                red(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<String>>(11)?,
                red(12)?,
                origin,
                origin_id,
                r.get::<_, Option<String>>(15)?,
                r.get::<_, Option<i64>>(16)?,
            ])?;
            if n == 1 {
                memory_ids.insert(id, here.last_insert_rowid());
            }
        }
    }

    // Evidence links, both ends remapped; links to events left out are dropped.
    {
        let mut st = theirs
            .prepare("SELECT memory_id, event_id, event_hash, relation FROM memory_evidence")?;
        let mut ins = here.prepare(
            "INSERT OR IGNORE INTO memory_evidence(memory_id, event_id, event_hash, relation) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for r in rows {
            let (m, e, hash, rel) = r?;
            if let (Some(m), Some(e)) = (memory_ids.get(&m), event_ids.get(&e)) {
                ins.execute(params![m, e, hash, rel])?;
            }
        }
    }

    // Incoming sessions are done: their memories came with them, and their transcripts
    // are not here to distil. Mark them so distillation never picks them up.
    {
        let mut through = here.prepare(
            "INSERT INTO distill_state(session_id, through, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO NOTHING",
        )?;
        let now = db::now_ms();
        let mut max_by_session: BTreeMap<String, i64> = BTreeMap::new();
        let mut st =
            here.prepare_cached("SELECT coalesce(max(id), 0) FROM events WHERE session_id = ?1")?;
        for s in &f.sessions {
            let m: i64 = st.query_row([s], |r| r.get(0))?;
            max_by_session.insert(s.clone(), m);
        }
        for (s, m) in max_by_session {
            through.execute(params![s, m, now])?;
        }
    }
    // Remember where this teammate's projects went, for the next merge with this prefix.
    let mut names = earlier_names(here, &f.summary.prefix)?;
    names.extend(f.projects.iter().map(|(a, b)| (a.clone(), b.clone())));
    here.execute(
        "INSERT INTO meta(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![
            format!("merge.projects.{}", f.summary.prefix),
            serde_json::to_string(&names)?
        ],
    )?;
    if memory_ids.len() != f.memories.len() {
        bail!(
            "merge stopped: {} of {} memories could be added; nothing was changed",
            memory_ids.len(),
            f.memories.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_ids_follow_projects_and_events() {
        assert_eq!(
            rewrite_origin_id(
                "user",
                "pin:github.com/a/b:abc",
                Some("ana/github.com/a/b"),
                None
            ),
            "pin:ana/github.com/a/b:abc"
        );
        assert_eq!(
            rewrite_origin_id("user", "pin:*:abc", Some("*"), None),
            "pin:*:abc"
        );
        let map: HashMap<i64, i64> = [(10, 510), (11, 511), (14, 514)].into_iter().collect();
        assert_eq!(
            rewrite_origin_id("mnem", "claude:s@10-14#2", Some("p"), Some(&map)),
            "claude:s@510-514#2"
        );
        // An end that was not copied falls back to the nearest copied id in the range.
        assert_eq!(
            rewrite_origin_id("mnem", "claude:s@9-15#0", Some("p"), Some(&map)),
            "claude:s@510-514#0"
        );
        assert_eq!(
            rewrite_origin_id("claude-mem", "obs:7", Some("p"), Some(&map)),
            "obs:7"
        );
    }

    #[test]
    fn shared_names_get_the_prefix() {
        assert_eq!(prefixed("ana", "github.com/a/b"), "ana/github.com/a/b");
        assert_eq!(prefixed("ana", "/home/ana/x"), "ana/home/ana/x");
    }

    #[test]
    fn prefixes_are_checked_and_defaulted() {
        assert_eq!(check_prefix(" ana/ ").unwrap(), "ana");
        assert!(check_prefix("a b").is_err());
        assert!(check_prefix("").is_err());
        assert!(check_prefix("../x").is_err());
        assert_eq!(default_prefix(Some("ana-laptop")), "ana-laptop");
        assert_eq!(default_prefix(Some("my host!")), "myhost");
        assert_eq!(default_prefix(None), "team");
    }
}
