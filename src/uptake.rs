//! Uptake: whether agents use what mnem gives them.
//!
//! The judged evals measure whether recalled memories *would* help. This measures what
//! happens in real sessions: every memory mnem injects (session start, prompt recall,
//! file recall) is recorded as an offer; every MCP call is recorded with the memory ids
//! it asked for; every hook run with its duration. `mnem uptake` then reports, per
//! source, how many offers were followed up: the memory fetched in full over MCP in the
//! same project within a day, or cited by id (`#123`) in the agent's replies in the same
//! session. Transcripts keep neither hook context nor MCP arguments, so this starts
//! counting when it is installed.

use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;

pub const SOURCES: &[&str] = &["start", "prompt", "file"];

/// Record that `ids` were put in front of `session`'s agent.
pub fn offered(conn: &Connection, session: &str, ids: &[i64], source: &str) -> Result<()> {
    let now = crate::db::now_ms();
    let mut st = conn.prepare_cached(
        "INSERT OR IGNORE INTO offers(session_id, memory_id, source, at) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for id in ids {
        st.execute(params![session, id, source, now])?;
    }
    Ok(())
}

/// Memory ids listed in injected text (`#123 ...` at the start of a line).
pub fn ids_in(text: &str) -> Vec<i64> {
    text.lines()
        .filter_map(|l| l.strip_prefix('#')?.split(' ').next()?.parse().ok())
        .collect()
}

/// Record an MCP call and the memory ids it asked for.
pub fn mcp_call(conn: &Connection, tool: &str, project: Option<&str>, ids: &[i64]) -> Result<()> {
    conn.execute(
        "INSERT INTO mcp_calls(at, tool, project, ids) VALUES (?1, ?2, ?3, ?4)",
        params![
            crate::db::now_ms(),
            tool,
            project,
            (!ids.is_empty()).then(|| serde_json::to_string(ids).unwrap_or_default())
        ],
    )?;
    Ok(())
}

/// Record how long a hook took.
pub fn hook_run(conn: &Connection, agent: &str, event: &str, ms: u128) -> Result<()> {
    conn.execute(
        "INSERT INTO hook_runs(at, agent, event, ms) VALUES (?1, ?2, ?3, ?4)",
        params![crate::db::now_ms(), agent, event, ms as i64],
    )?;
    Ok(())
}

/// One source's numbers over the window.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Source {
    pub offers: usize,
    pub sessions: usize,
    pub fetched: usize,
    pub cited: usize,
}

#[derive(Debug, Default)]
pub struct Report {
    pub since: Option<i64>,
    pub sources: Vec<(String, Source)>,
    /// MCP calls per tool.
    pub mcp: Vec<(String, usize)>,
    /// Hook runs per event: (count, p50 ms, p95 ms).
    pub hooks: Vec<(String, usize, i64, i64)>,
}

/// Does `text` cite memory `id` as `#id` (not as the start of a longer number)?
fn cites(text: &str, id: i64) -> bool {
    let needle = format!("#{id}");
    text.match_indices(&needle).any(|(i, _)| {
        !text[i + needle.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

pub fn report(conn: &Connection, days: i64) -> Result<Report> {
    let from = crate::db::now_ms() - days * 86_400_000;
    let since: Option<i64> = conn.query_row("SELECT min(at) FROM offers", [], |r| r.get(0))?;
    // Memory ids fetched over MCP, per project, with when.
    let mut fetches: HashMap<i64, Vec<(i64, Option<String>)>> = HashMap::new();
    {
        let mut st = conn
            .prepare("SELECT at, project, ids FROM mcp_calls WHERE ids IS NOT NULL AND at >= ?1")?;
        let rows = st.query_map([from], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (at, project, ids) = row?;
            for id in serde_json::from_str::<Vec<i64>>(&ids).unwrap_or_default() {
                fetches.entry(id).or_default().push((at, project.clone()));
            }
        }
    }
    let mut sources = Vec::new();
    for src in SOURCES {
        let mut st = conn.prepare(
            "SELECT o.session_id, o.memory_id, o.at, coalesce(s.project, '')
               FROM offers o LEFT JOIN sessions s ON s.id = o.session_id
              WHERE o.source = ?1 AND o.at >= ?2",
        )?;
        let offers: Vec<(String, i64, i64, String)> = st
            .query_map(params![src, from], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut replies: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        let mut out = Source {
            offers: offers.len(),
            ..Default::default()
        };
        let mut sessions = std::collections::HashSet::new();
        for (session, id, at, project) in &offers {
            sessions.insert(session.clone());
            let base = project.split('#').next().unwrap_or("");
            if fetches.get(id).is_some_and(|f| {
                f.iter().any(|(t, p)| {
                    *t >= *at
                        && *t - *at <= 86_400_000
                        && p.as_deref()
                            .is_none_or(|p| base.is_empty() || p.starts_with(base))
                })
            }) {
                out.fetched += 1;
            }
            let texts = match replies.get(session) {
                Some(t) => t,
                None => {
                    let mut q = conn.prepare_cached(
                        "SELECT coalesce(ts, 0), text FROM events
                          WHERE session_id = ?1 AND kind = 'assistant' AND text IS NOT NULL",
                    )?;
                    let t: Vec<(i64, String)> = q
                        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?;
                    replies.entry(session.clone()).or_insert(t)
                }
            };
            if texts.iter().any(|(t, text)| *t >= *at && cites(text, *id)) {
                out.cited += 1;
            }
        }
        out.sessions = sessions.len();
        sources.push((src.to_string(), out));
    }
    let mut st = conn.prepare(
        "SELECT tool, count(*) FROM mcp_calls WHERE at >= ?1 GROUP BY tool ORDER BY 2 DESC",
    )?;
    let mcp = st
        .query_map([from], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut st = conn.prepare("SELECT event, ms FROM hook_runs WHERE at >= ?1")?;
    let mut by_event: HashMap<String, Vec<i64>> = HashMap::new();
    for row in st.query_map([from], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (e, ms) = row?;
        by_event.entry(e).or_default().push(ms);
    }
    let mut hooks: Vec<(String, usize, i64, i64)> = by_event
        .into_iter()
        .map(|(e, mut v)| {
            v.sort_unstable();
            // Nearest rank: with few runs the slow one still shows as p95.
            let p = |q: f64| v[((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1];
            (e, v.len(), p(0.5), p(0.95))
        })
        .collect();
    hooks.sort();
    Ok(Report {
        since,
        sources,
        mcp,
        hooks,
    })
}

pub fn render(r: &Report, days: i64) -> String {
    let mut w = String::new();
    match r.since {
        Some(t) => w.push_str(&format!(
            "uptake over the last {days} days (recording since {} ago)\n",
            crate::context::ago(crate::db::now_ms() - t)
        )),
        None => {
            w.push_str("uptake: nothing recorded yet; it starts counting with the next sessions\n");
            return w;
        }
    }
    let label = |s: &str| match s {
        "start" => "session start",
        "prompt" => "prompt recall",
        "file" => "file recall",
        _ => "other",
    };
    for (src, s) in &r.sources {
        let pct = |n: usize| {
            if s.offers == 0 {
                "-".to_string()
            } else {
                format!("{:.0}%", 100.0 * n as f64 / s.offers as f64)
            }
        };
        w.push_str(&format!(
            "  {:<13} {:>5} memories offered in {:>3} sessions · fetched in full {:>4} ({}) · cited by id {:>4} ({})\n",
            label(src),
            s.offers,
            s.sessions,
            s.fetched,
            pct(s.fetched),
            s.cited,
            pct(s.cited)
        ));
    }
    if r.mcp.is_empty() {
        w.push_str("  MCP: no calls to mnem's tools\n");
    } else {
        let calls: Vec<String> = r.mcp.iter().map(|(t, n)| format!("{t} {n}")).collect();
        w.push_str(&format!("  MCP calls: {}\n", calls.join(", ")));
    }
    for (e, n, p50, p95) in &r.hooks {
        w.push_str(&format!(
            "  hook {e:<13} {n:>5} runs · p50 {p50} ms · p95 {p95} ms\n"
        ));
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_match_whole_ids_only() {
        assert!(cites("see #123 for why", 123));
        assert!(cites("(#123)", 123));
        assert!(!cites("see #1234", 123));
        assert!(!cites("123 without a hash", 123));
    }

    #[test]
    fn offers_are_followed_up_by_fetches_and_citations() {
        let c = crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, project) VALUES ('claude:s', 'claude', 's', 'github.com/o/r')",
            [],
        )
        .unwrap();
        offered(&c, "claude:s", &[1, 2, 3], "prompt").unwrap();
        offered(&c, "claude:s", &[4], "file").unwrap();
        // Memory 1 fetched over MCP in the same project; memory 2 cited in a reply.
        mcp_call(&c, "get_observations", Some("github.com/o/r"), &[1]).unwrap();
        c.execute(
            "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('claude:s', 'k', ?1, 'assistant', 'As #2 says, keep it bounded.')",
            [crate::db::now_ms() + 1],
        )
        .unwrap();
        hook_run(&c, "claude", "prompt", 20).unwrap();
        hook_run(&c, "claude", "prompt", 40).unwrap();
        let r = report(&c, 7).unwrap();
        let get = |s: &str| r.sources.iter().find(|x| x.0 == s).unwrap().1.clone();
        assert_eq!(
            get("prompt"),
            Source {
                offers: 3,
                sessions: 1,
                fetched: 1,
                cited: 1
            }
        );
        assert_eq!(get("file").offers, 1);
        assert_eq!(get("file").fetched + get("file").cited, 0);
        assert_eq!(r.mcp, vec![("get_observations".to_string(), 1)]);
        assert_eq!(r.hooks, vec![("prompt".to_string(), 2, 20, 40)]);
        assert!(render(&r, 7).contains("prompt recall"));
        assert_eq!(ids_in("mnem: x\n#12 a · b\n  #13 no\n#14 c"), vec![12, 14]);
    }
}
