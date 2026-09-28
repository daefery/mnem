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
/// A term found in fewer than this share of all memories carries real signal.
const RARE: f64 = 0.10;

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
/// FTS query matching memories with any of the text's meaningful words (None when
/// fewer than two): the fallback when a plain-language search matches no memory whole.
pub fn any_terms_query(text: &str) -> Option<String> {
    let terms = terms(text);
    (terms.len() >= 2).then(|| {
        terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Keyword,
    Vector,
    /// Reciprocal rank fusion of keyword and vector lists.
    Hybrid,
    /// Keyword order kept; vector hits only fill the remaining slots.
    Fill,
}

pub fn semantic_enabled() -> bool {
    crate::config::CONFIG.semantic.enabled != Some(false)
}

/// The semantic model loaded in this process (for eval and CLI use).
pub fn semantic_embedder() -> Option<crate::embed::Embedder> {
    semantic_enabled()
        .then(|| crate::embed::Embedder::load().ok())
        .flatten()
}

type Hit = (i64, String, String, i64);

/// Rank memories for a prompt by keywords, by meaning, or both fused (reciprocal rank
/// fusion). Without an embedder every mode is keyword ranking.
pub fn rank(
    conn: &Connection,
    project: &str,
    prompt: &str,
    exclude_session: Option<&str>,
    limit: usize,
    query: Option<&crate::embed::Query>,
    mode: Mode,
) -> Result<Vec<Hit>> {
    let Some(q) = query.filter(|_| mode != Mode::Keyword) else {
        return keyword_rank(conn, project, prompt, exclude_session, limit);
    };
    // Same gate as keywords: harness text and very short prompts carry no query.
    let Some((clean, label)) = classify_prompt(prompt) else {
        return Ok(vec![]);
    };
    if label.is_some() || clean.split_whitespace().count() < 4 {
        return Ok(vec![]);
    }
    if mode == Mode::Fill {
        let mut out = keyword_rank(conn, project, prompt, exclude_session, limit)?;
        fill_with_vectors(conn, q, project, exclude_session, &mut out, limit)?;
        return Ok(out);
    }
    let pool = limit * 6;
    let vector = vector_hits(
        conn,
        q,
        project,
        exclude_session,
        pool,
        if mode == Mode::Vector {
            0.0
        } else {
            MIN_COSINE
        },
    )?;
    if mode == Mode::Vector {
        return Ok(vector.into_iter().take(limit).collect());
    }
    let keyword = keyword_rank(conn, project, prompt, exclude_session, pool)?;
    let ids = fuse(
        &keyword.iter().map(|h| h.0).collect::<Vec<_>>(),
        &vector.iter().map(|h| h.0).collect::<Vec<_>>(),
        VECTOR_WEIGHT,
    );
    Ok(ids
        .into_iter()
        .take(limit)
        .filter_map(|id| keyword.iter().chain(&vector).find(|h| h.0 == id).cloned())
        .collect())
}

/// Reciprocal rank fusion of a keyword ranking and a vector ranking, best first. With
/// a weight below 1 the vector list mostly reorders keyword hits and fills in after them.
pub fn fuse(keyword: &[i64], vector: &[i64], vector_weight: f64) -> Vec<i64> {
    let mut fused: Vec<(f64, i64)> = Vec::new();
    for (list, weight) in [(keyword, 1.0), (vector, vector_weight)] {
        for (r, id) in list.iter().enumerate() {
            let score = weight / (RRF_K + r as f64 + 1.0);
            match fused.iter_mut().find(|(_, x)| x == id) {
                Some((s, _)) => *s += score,
                None => fused.push((score, *id)),
            }
        }
    }
    fused.sort_by(|a, b| b.0.total_cmp(&a.0));
    fused.into_iter().map(|(_, id)| id).collect()
}

/// Vector hits above `min_cos`, with the fields recall shows.
fn vector_hits(
    conn: &Connection,
    q: &crate::embed::Query,
    project: &str,
    exclude_session: Option<&str>,
    limit: usize,
    min_cos: f32,
) -> Result<Vec<Hit>> {
    let mut info = conn.prepare_cached(
        "SELECT coalesce(type, kind), coalesce(title, ''), coalesce(created_at, 0) FROM memories WHERE id = ?1",
    )?;
    let mut out = Vec::new();
    for (id, cos) in crate::embed::search(conn, q, project, exclude_session, limit)? {
        if cos < min_cos {
            break;
        }
        out.push(info.query_row([id], |r| Ok((id, r.get(0)?, r.get(1)?, r.get(2)?)))?);
    }
    Ok(out)
}

/// Fill empty slots in `out` with meaning-based hits that clear MIN_COSINE. An empty
/// slot is better than an unrelated memory.
pub fn fill_with_vectors(
    conn: &Connection,
    q: &crate::embed::Query,
    project: &str,
    exclude_session: Option<&str>,
    out: &mut Vec<Hit>,
    limit: usize,
) -> Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    for h in vector_hits(conn, q, project, exclude_session, limit * 2, MIN_COSINE)? {
        if out.len() >= limit {
            break;
        }
        if !out.iter().any(|x| x.0 == h.0) {
            out.push(h);
        }
    }
    Ok(())
}

/// Reciprocal-rank-fusion constant (the usual 60).
const RRF_K: f64 = 60.0;
/// Weight of the vector list relative to keywords in hybrid fusion (best in `mnem eval`).
const VECTOR_WEIGHT: f64 = 0.5;
/// The same for explicit search (MCP `search`, viewer): on the eval questions sent
/// through MCP search, 0.2 beat keywords alone at hit@1 (62% vs 52%) without losing any
/// target from the top 5 or 20; 0.5 and above pushed targets out of the top 5.
pub const SEARCH_VECTOR_WEIGHT: f64 = 0.2;
/// Vector hits below this cosine similarity are not offered. With potion-base-8M, true
/// targets and the best wrong candidate have nearly the same cosine distribution
/// (median 0.63 each, `mnem eval --cosines`), so vectors only fill slots; 0.55 (about
/// the 10th percentile of true targets) trims the weakest filler.
pub const MIN_COSINE: f32 = 0.55;

/// Up to TOP unseen memories in `project` matching `prompt`, formatted for injection.
/// Keyword ranking: memory ids in `project` for `prompt`, best first; `exclude_session`
/// hides memories already offered to that session. Empty when the prompt carries no query.
pub fn keyword_rank(
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
    // One shared common word is not relevance: need at least two prompt terms in the
    // memory, and at least one of the prompt's rarer terms in its title or subtitle.
    let need = terms.len().min(2);
    let total: f64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get::<_, i64>(0))?
        .max(1) as f64;
    let mut df =
        conn.prepare_cached("SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1")?;
    let rare: Vec<&String> = terms
        .iter()
        .filter(|t| {
            df.query_row([format!("\"{t}\"")], |r| r.get::<_, i64>(0))
                .map(|n| (n as f64) / total < RARE)
                .unwrap_or(false)
        })
        .collect();
    if rare.is_empty() {
        return Ok(vec![]);
    }
    let mut st = conn.prepare_cached(
        "SELECT m.id, coalesce(m.type, m.kind), coalesce(m.title, ''), coalesce(m.created_at, 0),
                lower(coalesce(m.title, '') || ' ' || coalesce(m.subtitle, '') || ' ' ||
                      coalesce(m.narrative, '') || ' ' || coalesce(m.facts, '')),
                lower(coalesce(m.title, '') || ' ' || coalesce(m.subtitle, ''))
         FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
         WHERE memories_fts MATCH ?1 AND m.project = ?2 AND m.kind != 'pinned'
           -- Personal details stay out of automatic injection; explicit search still finds them.
           AND coalesce(m.type, '') != 'sensitive'
           AND NOT EXISTS (SELECT 1 FROM recall_seen r WHERE r.session_id = ?3 AND r.memory_id = m.id)
         -- Column weights (title, subtitle, narrative, facts, concepts) chosen with `mnem eval`.
         ORDER BY bm25(memories_fts, 5.0, 3.0, 1.0, 1.5, 1.0) + (strftime('%s', 'now') * 1000 - m.created_at) / 2.592e10
         LIMIT ?4",
    )?;
    let rows = st
        .query_map(
            params![
                query,
                project,
                exclude_session.unwrap_or(""),
                (limit * 4) as i64
            ],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )?
        .filter_map(Result::ok)
        .filter(|row: &(i64, String, String, i64, String)| {
            terms.iter().filter(|t| row.4.contains(t.as_str())).count() >= need
        })
        .take(limit)
        .map(|(id, kind, title, at, _)| (id, kind, title, at))
        .collect();
    Ok(rows)
}

/// Up to TOP unseen memories in `project` matching `prompt`, formatted for injection.
pub fn recall(
    conn: &Connection,
    session: &str,
    project: &str,
    prompt: &str,
) -> Result<Option<String>> {
    // Keywords first (they score best at top five in `mnem eval`). Only when they leave
    // slots empty is the watch service asked for a query vector.
    let mut rows = keyword_rank(conn, project, prompt, Some(session), TOP)?;
    if rows.len() < TOP
        && semantic_enabled()
        && classify_prompt(prompt)
            .is_some_and(|(c, l)| l.is_none() && c.split_whitespace().count() >= 4)
        && let Some(q) = crate::embed::query_from_service(conn, prompt)
    {
        fill_with_vectors(conn, &q, project, Some(session), &mut rows, TOP)?;
    }
    if rows.is_empty() {
        return Ok(None);
    }
    let now = db::now_ms();
    let mut w = String::from(
        "mnem recall: past memories matching this prompt (full text: get_observations([ids]))\n",
    );
    let mut shown = Vec::new();
    let mut openings: Vec<String> = Vec::new();
    for (id, kind, title, at) in rows {
        // Imported history holds near-identical summaries; one of them is enough.
        let opening: String = title.to_lowercase().chars().take(48).collect();
        if openings.contains(&opening) {
            continue;
        }
        openings.push(opening);
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
