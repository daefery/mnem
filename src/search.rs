use anyhow::Result;
use rusqlite::{Connection, params};

/// Quote each term so user input can never be parsed as FTS5 syntax.
pub fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
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
