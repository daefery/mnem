//! Scripted sessions: sessions driven by another agent's brief (a review council, a
//! test run) rather than by a person. Their prompts match `scripted_sessions` patterns in
//! the settings. They are not distilled, not offered memories, left out of uptake, and
//! the memories already made from them stay out of recall and search. Nothing is
//! deleted: change the patterns and the next scan puts them back.

use crate::config::CONFIG;
use crate::db;
use anyhow::Result;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::LazyLock;

/// For a query over `memories m`: the memory was not made from a scripted session.
pub const MEMORY_NOT_SCRIPTED: &str =
    "NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = m.session_id)";

/// For a query over `sessions s`: the session is not scripted.
pub const SESSION_NOT_SCRIPTED: &str =
    "NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = s.id)";

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    CONFIG
        .scripted_sessions
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("mnem: bad scripted_sessions pattern {p:?}: {e}");
                None
            }
        })
        .collect()
});

/// Whether a prompt is one another agent's brief would send.
pub fn matches(prompt: &str) -> bool {
    matches_any(&PATTERNS, prompt)
}

fn matches_any(patterns: &[Regex], prompt: &str) -> bool {
    let p = prompt.trim();
    patterns.iter().any(|r| r.is_match(p))
}

pub fn mark(conn: &Connection, session: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO scripted_sessions(session_id, at) VALUES (?1, ?2)",
        params![session, db::now_ms()],
    )?;
    Ok(())
}

/// Whether `session` is scripted. A database error counts as scripted: pushing memories
/// into a session that may follow a brief is the worse mistake.
pub fn is_scripted(conn: &Connection, session: &str) -> bool {
    match conn
        .query_row(
            "SELECT 1 FROM scripted_sessions WHERE session_id = ?1",
            [session],
            |_| Ok(()),
        )
        .optional()
    {
        Ok(found) => found.is_some(),
        Err(e) => {
            crate::hook::log(&format!("scripted: {e:#}"));
            true
        }
    }
}

/// Mark the sessions whose prompts match, scanning only prompts not seen yet; when the
/// patterns change, forget every mark and scan everything again. Returns sessions marked.
pub fn refresh(conn: &Connection) -> Result<usize> {
    refresh_with(
        conn,
        &PATTERNS,
        &CONFIG.scripted_sessions.join("\u{1f}"),
        // Settings older than the patterns another process published (a long-running
        // watcher keeps what it read at start) leave the marks alone.
        CONFIG.loaded_at,
    )
}

fn refresh_with(conn: &Connection, patterns: &[Regex], key: &str, loaded_at: i64) -> Result<usize> {
    let meta = |k: &str| -> Result<Option<String>> {
        Ok(conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .optional()?)
    };
    if meta("scripted.patterns")?.as_deref() != Some(key) {
        let published: i64 = meta("scripted.patterns_at")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if published > loaded_at {
            return Ok(0);
        }
        // New patterns: every mark, the patterns and the scan position change together,
        // so a scan cut short resumes from the start instead of skipping prompts.
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM scripted_sessions", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('scripted.patterns', ?1), ('scripted.patterns_at', ?2), ('scripted.through', '0')",
            params![key, db::now_ms().to_string()],
        )?;
        tx.commit()?;
    }
    let mut from: i64 = meta("scripted.through")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut marked = 0;
    loop {
        let rows: Vec<(i64, String, String)> = {
            let mut st = conn.prepare_cached(
                "SELECT id, session_id, coalesce(text, '') FROM events
                  WHERE id > ?1 AND kind = 'prompt' AND thread IS NULL ORDER BY id LIMIT 5000",
            )?;
            st.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let Some(last) = rows.last().map(|r| r.0) else {
            break;
        };
        // Matching happens outside the write; each batch's marks and position commit together.
        let hits: Vec<&String> = rows
            .iter()
            .filter(|r| matches_any(patterns, &r.2))
            .map(|r| &r.1)
            .collect();
        let tx = conn.unchecked_transaction()?;
        for session in hits {
            marked += tx.execute(
                "INSERT OR IGNORE INTO scripted_sessions(session_id, at) VALUES (?1, ?2)",
                params![session, db::now_ms()],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('scripted.through', ?1)",
            [last.to_string()],
        )?;
        tx.commit()?;
        from = last;
    }
    Ok(marked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        for (i, (session, text)) in [
            (
                "pi:council",
                "Round 41. Read /tmp/claude-1/brief.md fully and do your role's part.",
            ),
            ("pi:council", "Round 42. Read /tmp/claude-1/brief.md fully."),
            ("claude:me", "fix the retry loop in fetch.rs"),
        ]
        .into_iter()
        .enumerate()
        {
            c.execute(
                "INSERT INTO events(session_id, record_key, kind, text) VALUES (?1, ?2, 'prompt', ?3)",
                params![session, format!("k{i}"), text],
            )
            .unwrap();
        }
        c
    }

    fn marked(c: &Connection) -> Vec<String> {
        let mut st = c
            .prepare("SELECT session_id FROM scripted_sessions ORDER BY 1")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn sessions_with_a_briefed_prompt_are_marked_and_patterns_can_change() {
        let c = db();
        let round = [Regex::new(r"^Round \d+\. Read /tmp/").unwrap()];
        assert_eq!(refresh_with(&c, &round, "a", i64::MAX).unwrap(), 1);
        assert_eq!(marked(&c), ["pi:council"]);
        // Only new prompts are scanned on the next pass.
        c.execute(
            "INSERT INTO events(session_id, record_key, kind, text) VALUES ('pi:test', 'k9', 'prompt', 'Round 7. Read /tmp/x')",
            [],
        )
        .unwrap();
        assert_eq!(refresh_with(&c, &round, "a", i64::MAX).unwrap(), 1);
        assert_eq!(marked(&c), ["pi:council", "pi:test"]);
        // A process that read its settings before these patterns were published (a
        // long-running watcher) leaves the marks alone.
        let none: [Regex; 0] = [];
        assert_eq!(refresh_with(&c, &none, "", 0).unwrap(), 0);
        assert_eq!(marked(&c).len(), 2);
        // Newer settings: every mark is reconsidered.
        assert_eq!(refresh_with(&c, &none, "", i64::MAX).unwrap(), 0);
        assert!(marked(&c).is_empty());
        // A rescan cut short (position reset, marks gone) picks up every prompt again.
        c.execute("UPDATE meta SET v = '0' WHERE k = 'scripted.through'", [])
            .unwrap();
        c.execute("DELETE FROM meta WHERE k = 'scripted.patterns'", [])
            .unwrap();
        assert_eq!(refresh_with(&c, &round, "a", i64::MAX).unwrap(), 2);
        assert!(!matches_any(&round, "please fix Round 41 of the tests"));
    }

    #[test]
    fn memories_from_scripted_sessions_are_left_out() {
        let c = db();
        for (id, session) in [(1, "pi:council"), (2, "claude:me")] {
            c.execute(
                "INSERT INTO memories(id, session_id, kind, title, origin, origin_id) VALUES (?1, ?2, 'observation', 't', 'mnem', ?1)",
                params![id, session],
            )
            .unwrap();
        }
        mark(&c, "pi:council").unwrap();
        let ids: Vec<i64> = c
            .prepare(&format!(
                "SELECT id FROM memories m WHERE {MEMORY_NOT_SCRIPTED}"
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, [2]);
        assert!(is_scripted(&c, "pi:council") && !is_scripted(&c, "claude:me"));
    }
}
