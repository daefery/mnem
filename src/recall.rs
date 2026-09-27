//! Prompt-time recall: memories that match what the user just asked.
//!
//! Session-start context is ordered by recency; this adds relevance. The prompt's key
//! terms and any file paths in it are matched against distilled memories in the same
//! project. Each memory is offered at most once per session, and the result is capped
//! so it never crowds out the prompt itself.

use crate::adapters::classify_prompt;
use crate::context::ago;
use crate::db;
use crate::text;
use anyhow::Result;
use rusqlite::{Connection, params};

const TOP: usize = 5;
const MAX_CHARS: usize = 600;
const MAX_TERMS: usize = 12;

const STOP: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "you", "your", "are", "was", "were", "have",
    "has", "had", "not", "but", "can", "could", "would", "should", "will", "what", "when", "where",
    "which", "who", "why", "how", "into", "from", "then", "than", "them", "they", "there", "their",
    "just", "also", "like", "make", "made", "want", "need", "please", "let", "lets", "use",
    "using", "used", "now", "yes", "okay", "sure", "does", "did", "done", "all", "any", "some",
    "more", "most", "one", "two", "our", "out", "its", "about", "after", "before", "again",
    "still", "only", "yang", "dan", "untuk", "dengan", "ini", "itu", "ada", "bisa", "tidak",
    "juga", "saya", "kita", "apa",
];

/// Search terms from a prompt: distinctive words and file-ish tokens, longest first.
pub fn terms(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in prompt.split(|c: char| c.is_whitespace() || ",;:()[]{}\"'`<>!?".contains(c)) {
        // Paths and file names: keep the last path component, which FTS indexes as words.
        let tok = raw
            .trim_matches(|c: char| !c.is_alphanumeric())
            .rsplit('/')
            .next()
            .unwrap_or("");
        for word in tok.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let w = word.to_lowercase();
            if w.len() >= 3
                && !STOP.contains(&w.as_str())
                && !w.chars().all(|c| c.is_ascii_digit())
                && !out.contains(&w)
            {
                out.push(w);
            }
        }
    }
    out.sort_by_key(|w| std::cmp::Reverse(w.len()));
    out.truncate(MAX_TERMS);
    out
}

/// Up to TOP unseen memories in `project` matching `prompt`, formatted for injection.
/// Memory ids in `project` ranked for `prompt`, best first; `exclude_session` hides
/// memories already offered to that session. Empty when the prompt carries no query.
pub fn rank(
    conn: &Connection,
    project: &str,
    prompt: &str,
    exclude_session: Option<&str>,
    limit: usize,
) -> Result<Vec<(i64, String, String, i64)>> {
    // Harness wrappers and very short prompts ("yes", "continue") carry no query.
    let Some((clean, label)) = classify_prompt(prompt) else {
        return Ok(vec![]);
    };
    if label.is_some() || clean.split_whitespace().count() < 4 {
        return Ok(vec![]);
    }
    let terms = terms(&clean);
    if terms.len() < 2 {
        return Ok(vec![]);
    }
    let query = terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut st = conn.prepare_cached(
        "SELECT m.id, coalesce(m.type, m.kind), coalesce(m.title, ''), coalesce(m.created_at, 0)
         FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
         WHERE memories_fts MATCH ?1 AND m.project = ?2 AND m.kind != 'pinned'
           AND NOT EXISTS (SELECT 1 FROM recall_seen r WHERE r.session_id = ?3 AND r.memory_id = m.id)
         -- Column weights (title, subtitle, narrative, facts, concepts) chosen with `mnem eval`.
         ORDER BY bm25(memories_fts, 5.0, 3.0, 1.0, 1.5, 1.0) + (strftime('%s', 'now') * 1000 - m.created_at) / 2.592e10
         LIMIT ?4",
    )?;
    let rows = st
        .query_map(
            params![query, project, exclude_session.unwrap_or(""), limit as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Up to TOP unseen memories in `project` matching `prompt`, formatted for injection.
pub fn recall(
    conn: &Connection,
    session: &str,
    project: &str,
    prompt: &str,
) -> Result<Option<String>> {
    let rows = rank(conn, project, prompt, Some(session), TOP)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let now = db::now_ms();
    let mut w = String::from(
        "mnem recall: past memories matching this prompt (full text: get_observations([ids]))\n",
    );
    let mut shown = Vec::new();
    for (id, kind, title, at) in rows {
        let line = format!(
            "#{id} {kind} · {} ago · {}\n",
            ago(now - at),
            text::head(&title, 110)
        );
        if w.len() + line.len() > MAX_CHARS {
            break;
        }
        w.push_str(&line);
        shown.push(id);
    }
    let mut mark = conn.prepare_cached(
        "INSERT OR IGNORE INTO recall_seen(session_id, memory_id) VALUES (?1, ?2)",
    )?;
    for id in &shown {
        mark.execute(params![session, id])?;
    }
    Ok((!shown.is_empty()).then_some(w))
}

#[cfg(test)]
mod tests {
    use super::terms;

    #[test]
    fn extracts_distinctive_terms() {
        let t = terms("why does the backup restore fail in src/backup.rs when watch is running?");
        assert!(t.contains(&"backup".to_string()) && t.contains(&"restore".to_string()));
        assert!(t.contains(&"running".to_string()));
        assert!(!t.contains(&"the".to_string()) && !t.contains(&"why".to_string()));
        assert!(!t.contains(&"rs".to_string()));
    }
}
