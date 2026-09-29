//! Keeping secrets out of what mnem already stored. New text is redacted as it is
//! captured (`text::redact`); when the patterns grow (`text::REDACTION_VERSION`), this
//! redacts events, memories and session titles stored before, in batches, resuming where
//! it stopped. The transcripts on disk are the agents' own files and are left as they are.

use crate::text;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// Rows changed by one pass.
#[derive(Debug, Default, PartialEq)]
pub struct Redacted {
    pub events: usize,
    pub memories: usize,
    pub sessions: usize,
}

const BATCH: i64 = 2000;

fn meta(conn: &Connection, k: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok()))
}

fn set(conn: &Connection, k: &str, v: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES (?1, ?2)",
        params![k, v.to_string()],
    )?;
    Ok(())
}

/// Redact what was stored under older patterns; nothing to do once the stored version
/// is current. A new database starts current (everything in it was redacted on capture).
pub fn catch_up(conn: &Connection) -> Result<Redacted> {
    let mut done = Redacted::default();
    let version = match meta(conn, "redact.version")? {
        Some(v) => v,
        None => {
            let empty: bool =
                conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM events)", [], |r| r.get(0))?;
            if empty {
                set(conn, "redact.version", text::REDACTION_VERSION)?;
                return Ok(done);
            }
            1
        }
    };
    if version >= text::REDACTION_VERSION {
        return Ok(done);
    }
    // A pass for a newer target starts over; one cut short resumes where it stopped.
    if meta(conn, "redact.target")? != Some(text::REDACTION_VERSION) {
        let tx = conn.unchecked_transaction()?;
        set(&tx, "redact.target", text::REDACTION_VERSION)?;
        set(&tx, "redact.events_through", 0)?;
        set(&tx, "redact.memories_through", 0)?;
        tx.commit()?;
    }
    done.events = pass(
        conn,
        "redact.events_through",
        "SELECT id, coalesce(text, '') FROM events WHERE id > ?1 AND text IS NOT NULL ORDER BY id LIMIT ?2",
        |r| Ok((r.get(0)?, vec![r.get(1)?])),
        |tx, id, v| {
            tx.execute(
                "UPDATE events SET text = ?2 WHERE id = ?1",
                params![id, v[0]],
            )?;
            Ok(())
        },
    )?;
    done.memories = pass(
        conn,
        "redact.memories_through",
        "SELECT id, coalesce(title, ''), coalesce(subtitle, ''), coalesce(narrative, ''), coalesce(facts, '')
           FROM memories WHERE id > ?1 ORDER BY id LIMIT ?2",
        |r| Ok((r.get(0)?, vec![r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])),
        |tx, id, v| {
            // Empty stays NULL where it was NULL.
            let or_null = |s: &String| (!s.is_empty()).then(|| s.clone());
            tx.execute(
                "UPDATE memories SET title = ?2, subtitle = ?3, narrative = ?4, facts = ?5 WHERE id = ?1",
                params![id, v[0], or_null(&v[1]), or_null(&v[2]), or_null(&v[3])],
            )?;
            Ok(())
        },
    )?;
    let titles: Vec<(String, String)> = {
        let mut st = conn.prepare("SELECT id, title FROM sessions WHERE title IS NOT NULL")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let tx = conn.unchecked_transaction()?;
    for (id, title) in titles {
        let clean = text::redact(&title);
        if clean != title {
            tx.execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                params![id, clean],
            )?;
            done.sessions += 1;
        }
    }
    set(&tx, "redact.version", text::REDACTION_VERSION)?;
    tx.commit()?;
    if done != Redacted::default() {
        crate::hook::log(&format!(
            "privacy: redacted stored text again (patterns v{}): {} events, {} memories, {} session titles",
            text::REDACTION_VERSION,
            done.events,
            done.memories,
            done.sessions
        ));
    }
    Ok(done)
}

/// One table in id order: redact each row's fields and write back only the rows that
/// change, each batch together with its position.
fn pass(
    conn: &Connection,
    position: &str,
    select: &str,
    read: impl Fn(&rusqlite::Row) -> rusqlite::Result<(i64, Vec<String>)>,
    write: impl Fn(&Connection, i64, &[String]) -> Result<()>,
) -> Result<usize> {
    let mut from = meta(conn, position)?.unwrap_or(0);
    let mut changed = 0;
    loop {
        let rows: Vec<(i64, Vec<String>)> = {
            let mut st = conn.prepare_cached(select)?;
            st.query_map(params![from, BATCH], &read)?
                .collect::<rusqlite::Result<_>>()?
        };
        let Some(last) = rows.last().map(|r| r.0) else {
            break;
        };
        let tx = conn.unchecked_transaction()?;
        for (id, fields) in &rows {
            let clean: Vec<String> = fields.iter().map(|f| text::redact(f)).collect();
            if &clean != fields {
                write(&tx, *id, &clean)?;
                changed += 1;
            }
        }
        set(&tx, position, last)?;
        tx.commit()?;
        from = last;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn stored_text_is_redacted_again_once_and_resumes() {
        let c = db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        let url = "postgres://app:Xk29fqLm@db.prod.internal/main";
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, title) VALUES ('s', 'claude', 's', ?1)",
            [format!("connect {url}")],
        )
        .unwrap();
        for (i, t) in [format!("psql {url}"), "nothing secret here".to_string()]
            .iter()
            .enumerate()
        {
            c.execute(
                "INSERT INTO events(session_id, record_key, kind, text) VALUES ('s', ?1, 'command', ?2)",
                params![format!("k{i}"), t],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO memories(id, session_id, kind, title, narrative, origin, origin_id) VALUES (1, 's', 'observation', 'DB access', ?1, 'mnem', 'x')",
            [format!("Use {url} for reports")],
        )
        .unwrap();
        // Stored under the first patterns.
        c.execute("INSERT INTO meta(k, v) VALUES ('redact.version', '1')", [])
            .unwrap();
        let r = catch_up(&c).unwrap();
        assert_eq!(
            r,
            Redacted {
                events: 1,
                memories: 1,
                sessions: 1
            }
        );
        let all: String = c
            .query_row(
                "SELECT group_concat(t, ' ') FROM (SELECT text t FROM events UNION ALL SELECT narrative FROM memories UNION ALL SELECT title FROM sessions)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!all.contains("Xk29fqLm"), "{all}");
        // Search finds the memory by its redacted text, not by the password.
        let hits = |q: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!((hits("reports"), hits("Xk29fqLm")), (1, 0));
        // Done once: the next pass changes nothing.
        assert_eq!(catch_up(&c).unwrap(), Redacted::default());
    }
}
