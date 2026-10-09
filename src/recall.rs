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

/// Which memories a recall may use.
#[derive(Default, Clone, Copy)]
pub struct Scope<'a> {
    /// The session asking: memories distilled from it are left out, since the agent
    /// already has that conversation in context.
    pub session: Option<&'a str>,
    /// Memories already offered to this session are left out (never repeated).
    pub offered_to: Option<&'a str>,
    /// Only memories created before this time (ms): replaying an old prompt.
    pub before: Option<i64>,
}

impl<'a> Scope<'a> {
    /// Recall for a live session.
    pub fn session(session: &'a str) -> Scope<'a> {
        Scope {
            session: Some(session),
            offered_to: Some(session),
            before: None,
        }
    }
}

/// Rank memories for a prompt by keywords, by meaning, or both fused (reciprocal rank
/// fusion). Without an embedder every mode is keyword ranking.
pub fn rank(
    conn: &Connection,
    project: &str,
    prompt: &str,
    scope: &Scope,
    limit: usize,
    query: Option<&crate::embed::Query>,
    mode: Mode,
) -> Result<Vec<Hit>> {
    let Some(q) = query.filter(|_| mode != Mode::Keyword) else {
        return keyword_rank(conn, project, prompt, scope, limit);
    };
    // Same gate as keywords: harness text and very short prompts carry no query.
    let Some((clean, label)) = classify_prompt(prompt) else {
        return Ok(vec![]);
    };
    if label.is_some() || clean.split_whitespace().count() < 4 {
        return Ok(vec![]);
    }
    if mode == Mode::Fill {
        let mut out = keyword_rank(conn, project, prompt, scope, limit * 2)?;
        gate(conn, q, &mut out)?;
        out.truncate(limit);
        fill_with_vectors(conn, q, project, scope, &mut out, limit)?;
        return Ok(out);
    }
    let pool = limit * 6;
    let vector = vector_hits(
        conn,
        q,
        project,
        scope,
        pool,
        if mode == Mode::Vector {
            0.0
        } else {
            fill_cosine()
        },
    )?;
    if mode == Mode::Vector {
        return Ok(vector.into_iter().take(limit).collect());
    }
    let keyword = keyword_rank(conn, project, prompt, scope, pool)?;
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
    let mut score: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for (list, weight) in [(keyword, 1.0), (vector, vector_weight)] {
        for (r, id) in list.iter().enumerate() {
            let s = score.entry(*id).or_insert_with(|| {
                order.push(*id);
                0.0
            });
            *s += weight / (RRF_K + r as f64 + 1.0);
        }
    }
    // Stable: ties keep first-seen order.
    order.sort_by(|a, b| score[b].total_cmp(&score[a]));
    order
}

/// Vector hits above `min_cos`, with the fields recall shows.
fn vector_hits(
    conn: &Connection,
    q: &crate::embed::Query,
    project: &str,
    scope: &Scope,
    limit: usize,
    min_cos: f32,
) -> Result<Vec<Hit>> {
    let mut info = conn.prepare_cached(
        "SELECT coalesce(type, kind), coalesce(title, ''), coalesce(created_at, 0) FROM memories WHERE id = ?1",
    )?;
    let mut out = Vec::new();
    for (id, cos) in crate::embed::search(conn, q, project, scope, limit)? {
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
    scope: &Scope,
    out: &mut Vec<Hit>,
    limit: usize,
) -> Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    for h in vector_hits(conn, q, project, scope, limit * 2, fill_cosine())? {
        if out.len() >= limit {
            break;
        }
        if !out.iter().any(|x| x.0 == h.0) {
            out.push(h);
        }
    }
    Ok(())
}

/// Drop keyword hits whose meaning is far from the query: sharing words is not enough.
/// Memories without a vector yet (just distilled) are kept.
pub fn gate(conn: &Connection, q: &crate::embed::Query, rows: &mut Vec<Hit>) -> Result<()> {
    let ids: Vec<i64> = rows.iter().map(|h| h.0).collect();
    let cos = crate::embed::cosines(conn, q, &ids)?;
    let min = relevance_cosine();
    rows.retain(|h| cos.get(&h.0).is_none_or(|c| *c >= min));
    Ok(())
}

/// Keyword hits below this cosine to the query are dropped. On the model-written and
/// the hand-written vague eval sets together (`rvn eval --set vague`), prompts no
/// memory answers reached at most 0.37 (p90) while true targets rarely fell below 0.47
/// (p10): 0.45 cut prompts that recalled something anyway from 9 of 9 to 1, and kept 40
/// of the 43 targets keywords had found.
pub const RELEVANCE_COSINE: f32 = 0.45;

/// Thresholds (relevance, fill, search) for models tuned with `rvn eval --dump` on
/// judged real prompts; cosine scales differ between models. Others use potion-8M's.
fn model_thresholds() -> (f32, f32, f32) {
    match crate::embed::model_name().as_str() {
        // real-dev: gate 0.30 beat potion-8M at 0.45 on both helpful and unhelpful
        // memories; fill hardly matters; search peaked at 0.35 on the MCP eval.
        "fastembed:AllMiniLML6V2" => (0.30, 0.50, 0.35),
        _ => (
            RELEVANCE_COSINE,
            MIN_COSINE,
            crate::search::SOME_WORDS_COSINE,
        ),
    }
}

/// The thresholds in use: config overrides, else the model's defaults.
pub fn relevance_cosine() -> f32 {
    crate::config::CONFIG
        .semantic
        .relevance_cosine
        .unwrap_or(model_thresholds().0)
}
pub fn fill_cosine() -> f32 {
    crate::config::CONFIG
        .semantic
        .fill_cosine
        .unwrap_or(model_thresholds().1)
}
pub fn search_cosine() -> f32 {
    crate::config::CONFIG
        .semantic
        .search_cosine
        .unwrap_or(model_thresholds().2)
}

/// Reciprocal-rank-fusion constant (the usual 60).
const RRF_K: f64 = 60.0;
/// Weight of the vector list relative to keywords in hybrid fusion (best in `rvn eval`).
const VECTOR_WEIGHT: f64 = 0.5;
/// The same for explicit search (MCP `search`, viewer): on the eval questions sent
/// through MCP search, 0.2 beat keywords alone at hit@1 (62% vs 52%) without losing any
/// target from the top 5 or 20; 0.5 and above pushed targets out of the top 5.
pub const SEARCH_VECTOR_WEIGHT: f64 = 0.2;
/// Vector hits below this cosine similarity are not offered. With potion-base-8M, true
/// targets and the best wrong candidate have nearly the same cosine distribution
/// (median 0.63 each, `rvn eval --cosines`), so vectors only fill slots; 0.55 (about
/// the 10th percentile of true targets) trims the weakest filler.
pub const MIN_COSINE: f32 = 0.55;

/// Up to TOP unseen memories in `project` matching `prompt`, formatted for injection.
/// Keyword ranking: memory ids in `project` for `prompt`, best first, within `scope`.
/// Empty when the prompt carries no query. With `scope.before`, term rarity counts only
/// memories that existed then; BM25's own term statistics still cover the whole index
/// (FTS5 cannot restrict them), so replayed order is close to, not exactly, what recall
/// ranked at the time.
pub fn keyword_rank(
    conn: &Connection,
    project: &str,
    prompt: &str,
    scope: &Scope,
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
    // One shared common word is not relevance: a memory needs at least two prompt
    // terms, and the prompt at least one rarer term. (Also requiring a rare term in the
    // title was tried: flat judged precision on real-dev, fewer hits on the other sets.)
    let need = terms.len().min(2);
    // Rarity over the memories that exist in scope: a replayed prompt must not see
    // how common a word became later.
    let before = scope.before.unwrap_or(i64::MAX);
    let total: f64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE coalesce(created_at, 0) < ?1",
            [before],
            |r| r.get::<_, i64>(0),
        )?
        .max(1) as f64;
    let mut df = conn.prepare_cached(if scope.before.is_some() {
        "SELECT count(*) FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
         WHERE memories_fts MATCH ?1 AND coalesce(m.created_at, 0) < ?2"
    } else {
        "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1 AND ?2 = ?2"
    })?;
    let rare: Vec<&String> = terms
        .iter()
        .filter(|t| {
            df.query_row(params![format!("\"{t}\""), before], |r| r.get::<_, i64>(0))
                .map(|n| (n as f64) / total < RARE)
                .unwrap_or(false)
        })
        .collect();
    if rare.is_empty() {
        return Ok(vec![]);
    }
    let mut st = conn.prepare_cached(&format!(
        "SELECT m.id, coalesce(m.type, m.kind), coalesce(m.title, ''), coalesce(m.created_at, 0),
                lower(coalesce(m.title, '') || ' ' || coalesce(m.subtitle, '') || ' ' ||
                      coalesce(m.narrative, '') || ' ' || coalesce(m.facts, ''))
         FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
         WHERE memories_fts MATCH ?1 AND m.project = ?2 AND m.kind != 'pinned'
           -- Personal details stay out of automatic injection; explicit search still finds them.
           AND coalesce(m.type, '') != 'sensitive'
           AND NOT EXISTS (SELECT 1 FROM recall_seen r WHERE r.session_id = ?3 AND r.memory_id = m.id)
           AND (?5 = '' OR coalesce(m.session_id, '') != ?5) AND coalesce(m.created_at, 0) < ?6
           AND {not_scripted}
         -- Column weights (title, subtitle, narrative, facts, concepts) chosen with `rvn eval`.
         ORDER BY bm25(memories_fts, 5.0, 3.0, 1.0, 1.5, 1.0) + (strftime('%s', 'now') * 1000 - m.created_at) / 2.592e10
         LIMIT ?4",
        not_scripted = crate::scripted::MEMORY_NOT_SCRIPTED
    )
    )?;
    let rows = st
        .query_map(
            params![
                query,
                project,
                scope.offered_to.unwrap_or(""),
                (limit * 4) as i64,
                scope.session.unwrap_or(""),
                scope.before.unwrap_or(i64::MAX)
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

/// Whether `session` is in the trial half of a presentation trial: a fixed split by a
/// hash of the session id, so a session stays in one half and the split can be
/// recomputed later from the id alone.
pub fn trial_arm(session: &str) -> bool {
    use sha2::{Digest, Sha256};
    Sha256::digest(session.as_bytes())[0] & 1 == 1
}

/// How many memories prompt recall shows `session`: `trial_top` for the trial half,
/// else TOP (never more than TOP, never zero).
fn top_for(session: &str, trial_top: Option<usize>) -> usize {
    match trial_top {
        Some(n) if trial_arm(session) => n.clamp(1, TOP),
        _ => TOP,
    }
}

/// Up to TOP unseen memories in `project` matching `prompt`, formatted for injection.
pub fn recall(
    conn: &Connection,
    session: &str,
    project: &str,
    prompt: &str,
) -> Result<Option<String>> {
    // Keywords rank (they score best at top five in `rvn eval`); the query vector from
    // the watch service drops keyword hits that share words but not meaning, then fills
    // empty slots. Without the service, keywords alone.
    let scope = Scope::session(session);
    // Ranked to the usual five either way; a trial session is shown the best of them.
    let top = top_for(session, crate::config::CONFIG.recall.trial_top);
    let mut rows = keyword_rank(conn, project, prompt, &scope, TOP * 2)?;
    if semantic_enabled()
        && classify_prompt(prompt)
            .is_some_and(|(c, l)| l.is_none() && c.split_whitespace().count() >= 4)
        && let Some(q) = crate::embed::query_from_service(conn, prompt)
    {
        gate(conn, &q, &mut rows)?;
        rows.truncate(TOP);
        fill_with_vectors(conn, &q, project, &scope, &mut rows, TOP)?;
    }
    rows.truncate(top);
    if rows.is_empty() {
        return Ok(None);
    }
    let now = db::now_ms();
    let mut w = String::from(
        "ravnori recall: past memories matching this prompt (full text: get_observations([ids]))\n",
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
    // Recording what was shown is bookkeeping: when another writer holds the database,
    // the agent gets its memories now and the record is skipped (a memory may then be
    // offered again later) instead of the agent waiting on the lock.
    if let Err(e) = mark_shown(conn, session, &shown) {
        crate::hook::log(&format!("recall: shown not recorded ({e:#})"));
    }
    Ok((!shown.is_empty()).then_some(w))
}

/// Record that `session` was shown `ids`, in one write.
fn mark_shown(conn: &Connection, session: &str, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut mark = tx.prepare_cached(
            "INSERT OR IGNORE INTO recall_seen(session_id, memory_id) VALUES (?1, ?2)",
        )?;
        for id in ids {
            mark.execute(params![session, id])?;
        }
    }
    crate::uptake::offered(&tx, session, ids, "prompt")?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TOP, terms, top_for, trial_arm};

    #[test]
    fn a_trial_shows_fewer_memories_to_half_the_sessions_only() {
        let ids: Vec<String> = (0..200).map(|i| format!("pi:session-{i}")).collect();
        let trial = ids.iter().filter(|s| trial_arm(s)).count();
        assert!((70..=130).contains(&trial), "split {trial} of 200");
        for s in &ids {
            // The same session always lands in the same half.
            assert_eq!(trial_arm(s), trial_arm(&s.clone()));
            assert_eq!(top_for(s, None), TOP);
            assert_eq!(top_for(s, Some(2)), if trial_arm(s) { 2 } else { TOP });
            // Out-of-range settings stay within one and the usual five.
            assert_eq!(top_for(s, Some(0)), if trial_arm(s) { 1 } else { TOP });
            assert_eq!(top_for(s, Some(9)), TOP);
        }
    }

    #[test]
    fn extracts_distinctive_terms() {
        let t = terms("why does the backup restore fail in src/backup.rs when watch is running?");
        assert!(t.contains(&"backup".to_string()) && t.contains(&"restore".to_string()));
        assert!(t.contains(&"running".to_string()));
        assert!(!t.contains(&"the".to_string()) && !t.contains(&"why".to_string()));
        assert!(!t.contains(&"rs".to_string()));
    }
}
