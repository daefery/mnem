//! Forgetting and pinning.
//!
//! Forgetting deletes rows and leaves a tombstone in `forgotten`, keyed by something
//! stable across re-ingest (agent record keys, origin ids, session ids, project ids).
//! Backfill, import and distillation consult the tombstones, so a forgotten item never
//! comes back from a transcript replay or a re-import.
//!
//! Pinned memories are facts the user wants every agent to see: they open the
//! session-start context in their project (or everywhere, for global pins).

use crate::config::CONFIG;
use crate::db;
use crate::text;
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Default)]
pub struct Forgot {
    pub memories: usize,
    pub events: usize,
    pub sessions: usize,
}

fn tombstone(conn: &Connection, kind: &str, key: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO forgotten(kind, key, at) VALUES (?1, ?2, ?3)",
        params![kind, key, db::now_ms()],
    )?;
    Ok(())
}

fn delete_memory(conn: &Connection, id: i64) -> Result<bool> {
    let key: Option<String> = conn
        .query_row(
            "SELECT origin || ':' || origin_id FROM memories WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(key) = key else { return Ok(false) };
    tombstone(conn, "memory", &key)?;
    conn.execute("DELETE FROM memory_evidence WHERE memory_id = ?1", [id])?;
    conn.execute("DELETE FROM memories WHERE id = ?1", [id])?;
    Ok(true)
}

fn delete_event(conn: &Connection, id: i64) -> Result<bool> {
    let key: Option<String> = conn
        .query_row(
            "SELECT session_id || '|' || record_key FROM events WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(key) = key else { return Ok(false) };
    tombstone(conn, "event", &key)?;
    conn.execute("DELETE FROM memory_evidence WHERE event_id = ?1", [id])?;
    conn.execute("DELETE FROM events WHERE id = ?1", [id])?;
    Ok(true)
}

fn delete_session(conn: &Connection, session: &str, f: &mut Forgot) -> Result<()> {
    tombstone(conn, "session", session)?;
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM memories WHERE session_id = ?1")?
        .query_map([session], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        f.memories += delete_memory(conn, id)? as usize;
    }
    conn.execute(
        "DELETE FROM memory_evidence WHERE event_id IN (SELECT id FROM events WHERE session_id = ?1)",
        [session],
    )?;
    f.events += conn.execute("DELETE FROM events WHERE session_id = ?1", [session])?;
    for t in ["delta_seen", "recall_seen", "injections", "distill_state"] {
        let col = if t == "delta_seen" {
            "viewer"
        } else {
            "session_id"
        };
        conn.execute(&format!("DELETE FROM {t} WHERE {col} = ?1"), [session])?;
    }
    f.sessions += conn.execute("DELETE FROM sessions WHERE id = ?1", [session])?;
    Ok(())
}

/// Forget memories (`123`), events (`E123`), a whole session, or a whole project.
pub fn forget(
    conn: &mut Connection,
    ids: &[String],
    session: Option<&str>,
    project: Option<&str>,
) -> Result<Forgot> {
    let tx = conn.transaction()?;
    let mut f = Forgot::default();
    for raw in ids {
        let r = raw.trim().trim_start_matches('#');
        match r.strip_prefix(['E', 'e']) {
            Some(n) => f.events += delete_event(&tx, n.parse()?)? as usize,
            None => f.memories += delete_memory(&tx, r.parse()?)? as usize,
        }
    }
    if let Some(s) = session {
        delete_session(&tx, s, &mut f)?;
    }
    if let Some(p) = project {
        tombstone(&tx, "project", p)?;
        let sessions: Vec<String> = tx
            .prepare("SELECT id FROM sessions WHERE project = ?1")?
            .query_map([p], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for s in sessions {
            delete_session(&tx, &s, &mut f)?;
        }
        let ids: Vec<i64> = tx
            .prepare("SELECT id FROM memories WHERE project = ?1")?
            .query_map([p], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            f.memories += delete_memory(&tx, id)? as usize;
        }
    }
    tx.commit()?;
    Ok(f)
}

/// Is this session (or its project) forgotten or excluded by config?
pub fn session_blocked(conn: &Connection, session: &str, project: Option<&str>) -> Result<bool> {
    if let Some(p) = project
        && CONFIG
            .exclude_projects
            .iter()
            .any(|x| !x.is_empty() && p.contains(x.as_str()))
    {
        return Ok(true);
    }
    Ok(conn
        .query_row(
            "SELECT 1 FROM forgotten WHERE (kind = 'session' AND key = ?1) OR (kind = 'project' AND key = ?2)",
            params![session, project.unwrap_or("")],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn event_forgotten(conn: &Connection, session: &str, record_key: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM forgotten WHERE kind = 'event' AND key = ?1")?
        .query_row([format!("{session}|{record_key}")], |_| Ok(()))
        .optional()?
        .is_some())
}

pub fn memory_forgotten(conn: &Connection, origin: &str, origin_id: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM forgotten WHERE kind = 'memory' AND key = ?1")?
        .query_row([format!("{origin}:{origin_id}")], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Pin a fact. `project` None pins it for every project.
pub fn remember(conn: &Connection, fact: &str, project: Option<&str>) -> Result<i64> {
    let fact = text::redact(fact.trim());
    if fact.is_empty() {
        bail!("nothing to remember");
    }
    let scope = project.unwrap_or("*");
    conn.execute(
        "INSERT INTO memories(project, kind, type, title, narrative, origin, origin_id, created_at)
         VALUES (?1, 'pinned', 'decision', ?2, ?2, 'user', ?3, ?4)
         ON CONFLICT(origin, origin_id) DO UPDATE SET created_at = excluded.created_at",
        params![
            scope,
            text::head(&fact, 500),
            format!("pin:{}:{}", scope, text::hash(&fact)),
            db::now_ms()
        ],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM memories WHERE origin = 'user' AND origin_id = ?1",
        [format!("pin:{}:{}", scope, text::hash(&fact))],
        |r| r.get(0),
    )?)
}

/// Pinned facts for a project, project pins first, then global ones.
pub fn pinned(conn: &Connection, project: &str) -> Result<Vec<(i64, String)>> {
    Ok(conn
        .prepare(
            "SELECT id, coalesce(narrative, title) FROM memories WHERE kind = 'pinned' AND project IN (?1, '*')
             ORDER BY project = '*', created_at DESC LIMIT 20",
        )?
        .query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}
