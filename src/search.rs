use anyhow::Result;
use rusqlite::{Connection, params};

/// Quote each term so user input can never be parsed as FTS5 syntax.
pub fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Most candidates either list contributes to a relevance ranking. The ranking is
/// computed the same way for every page, so pages never overlap or shift.
pub const POOL: usize = 1000;

/// A memory in a relevance ranking and what matched it.
pub struct Ranked {
    pub id: i64,
    pub by_words: bool,
    pub by_meaning: bool,
}

impl Ranked {
    pub fn how(&self) -> &'static str {
        match (self.by_words, self.by_meaning) {
            (true, true) => "both",
            (false, true) => "meaning",
            _ => "words",
        }
    }
}

/// Memories matching `raw`, best first, among memories `m` passing the SQL condition
/// `filter` (its `?` placeholders bound by `args()`). Memories with every word come
/// first, then ones with any meaningful word (a plain-language question rarely matches
/// whole), fused with the nearest memories by meaning when a query vector is given.
pub fn rank_memories(
    conn: &Connection,
    raw: &str,
    vq: Option<&crate::embed::Query>,
    filter: &str,
    args: &dyn Fn() -> Vec<Box<dyn rusqlite::ToSql>>,
) -> Result<Vec<Ranked>> {
    let mut st = conn.prepare(&format!(
        "SELECT m.id FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
         WHERE memories_fts MATCH ? AND ({filter})
         ORDER BY bm25(memories_fts) + (strftime('%s','now') * 1000 - m.created_at) / 2.592e9
         LIMIT {POOL}"
    ))?;
    let mut matching = |fq: &str| -> Result<Vec<i64>> {
        let mut all: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fq.to_string())];
        all.extend(args());
        Ok(st
            .query_map(
                rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?)
    };
    let every = fts_query(raw);
    let mut words = if every.is_empty() {
        vec![]
    } else {
        matching(&every)?
    };
    if words.len() < POOL
        && let Some(any) = crate::recall::any_terms_query(raw)
    {
        for id in matching(&any)? {
            if words.len() >= POOL {
                break;
            }
            if !words.contains(&id) {
                words.push(id);
            }
        }
    }
    let meaning: Vec<i64> = match vq {
        Some(q) => crate::embed::search_where(conn, q, filter, args(), POOL)?
            .into_iter()
            .take_while(|(_, cos)| *cos >= crate::recall::MIN_COSINE)
            .map(|(id, _)| id)
            .collect(),
        None => vec![],
    };
    let words_set: std::collections::HashSet<i64> = words.iter().copied().collect();
    let meaning_set: std::collections::HashSet<i64> = meaning.iter().copied().collect();
    Ok(
        crate::recall::fuse(&words, &meaning, crate::recall::SEARCH_VECTOR_WEIGHT)
            .into_iter()
            .map(|id| Ranked {
                id,
                by_words: words_set.contains(&id),
                by_meaning: meaning_set.contains(&id),
            })
            .collect(),
    )
}

pub fn run(conn: &Connection, query: &str, project: Option<&str>, limit: usize) -> Result<()> {
    let q = fts_query(query);
    if q.is_empty() {
        anyhow::bail!("empty query");
    }
    let mut stmt = conn.prepare(
        "SELECT datetime(e.ts / 1000, 'unixepoch'), s.agent, coalesce(s.project, '?'), e.kind,
                coalesce(e.path, ''), snippet(events_fts, 0, '[', ']', '…', 16)
         FROM events_fts JOIN events e ON e.id = events_fts.rowid JOIN sessions s ON s.id = e.session_id
         WHERE events_fts MATCH ?1 AND (?2 IS NULL OR s.project LIKE '%' || ?2 || '%')
         ORDER BY bm25(events_fts) + (strftime('%s', 'now') * 1000 - e.ts) / 8.64e9
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![q, project, limit as i64], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (ts, agent, project, kind, path, snip) = row?;
        let snip = snip.replace('\n', " ");
        let loc = if path.is_empty() {
            String::new()
        } else {
            format!(" {path}")
        };
        println!("{ts} {agent:<6} {project} {kind}{loc}\n    {snip}");
    }
    Ok(())
}
