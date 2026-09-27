//! Context injected at session start: what happened recently in this project, across
//! every agent, within a fixed character budget.

use crate::db;
use crate::text;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt::Write;

pub struct Options<'a> {
    pub project: &'a str,
    /// mnem session id of the caller ("claude:<uuid>"), excluded from "recent".
    pub current: Option<&'a str>,
    pub budget_chars: usize,
    pub sessions: usize,
    pub turns: usize,
    pub observations: usize,
}

struct Turn {
    prompt: String,
    answer: Option<String>,
}

struct SessionView {
    agent: String,
    title: Option<String>,
    last: i64,
    turns: Vec<Turn>,
    edited: Vec<String>,
    last_error: Option<String>,
}

pub fn build(conn: &Connection, o: &Options) -> Result<String> {
    let sessions = recent_sessions(conn, o)?;
    let (obs, summary) = memories(conn, o.project, o.observations)?;
    // Shrink until it fits: fewer observations, then fewer turns, then fewer sessions.
    let (mut n_obs, mut n_turns, mut n_sess) = (obs.len(), o.turns, sessions.len());
    loop {
        let out = render(
            o,
            &sessions[..n_sess],
            n_turns,
            &obs[..n_obs],
            summary.as_ref(),
        )?;
        if out.len() <= o.budget_chars || (n_obs == 0 && n_turns <= 1 && n_sess <= 1) {
            return Ok(out);
        }
        if n_obs > 10 {
            n_obs /= 2;
        } else if n_turns > 1 {
            n_turns -= 1;
        } else if n_sess > 1 {
            n_sess -= 1;
        } else {
            n_obs = 0;
        }
    }
}

/// id, agent, title, last event, human prompt count, first prompt
type Candidate = (
    String,
    String,
    Option<String>,
    Option<i64>,
    i64,
    Option<String>,
);

fn recent_sessions(conn: &Connection, o: &Options) -> Result<Vec<SessionView>> {
    // Sessions a human actually talked to come first; orchestrator-only sessions (often
    // many near-identical subagents) fill in only when there is room.
    let mut s = conn.prepare(
        "SELECT id, agent, title, last_event_at,
                (SELECT count(*) FROM events e WHERE e.session_id = sessions.id AND e.kind = 'prompt'
                   AND e.label IS NULL AND e.thread IS NULL) AS human,
                (SELECT text FROM events e WHERE e.session_id = sessions.id AND e.kind = 'prompt'
                   AND e.thread IS NULL ORDER BY e.id LIMIT 1) AS first_prompt
         FROM sessions
         WHERE project = ?1 AND id IS NOT ?2
           AND EXISTS (SELECT 1 FROM events e WHERE e.session_id = sessions.id AND e.kind = 'prompt')
         ORDER BY last_event_at DESC LIMIT ?3",
    )?;
    let candidates: Vec<Candidate> = s
        .query_map(
            params![o.project, o.current, (o.sessions * 6) as i64],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut rows = Vec::new();
    let mut first_prompts = std::collections::HashSet::new();
    for human_pass in [true, false] {
        for (id, agent, title, last, human, first) in &candidates {
            if rows.len() == o.sessions {
                break;
            }
            if (*human > 0) != human_pass {
                continue;
            }
            // Many subagents start from the same brief; one of them is enough.
            // Briefs differ only by a file name deep in the text; compare their openings.
            if let Some(f) = first
                && !first_prompts.insert(f.chars().take(40).collect::<String>().to_lowercase())
            {
                continue;
            }
            rows.push((id.clone(), agent.clone(), title.clone(), *last));
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.3));
    let mut prompts = conn.prepare_cached(
        "SELECT turn, text FROM events
         WHERE session_id = ?1 AND kind = 'prompt' AND thread IS NULL
         ORDER BY (label IS NULL) DESC, id DESC LIMIT ?2",
    )?;

    let mut edited = conn.prepare_cached(
        "SELECT path FROM events WHERE session_id = ?1 AND kind = 'file_edit'
         GROUP BY path ORDER BY max(id) DESC LIMIT 8",
    )?;
    let mut last_event = conn.prepare_cached(
        "SELECT kind, label, text FROM events WHERE session_id = ?1 AND thread IS NULL ORDER BY id DESC LIMIT 1",
    )?;
    let mut out = Vec::new();
    for (id, agent, title, last) in rows {
        let mut turns: Vec<(i64, String)> = prompts
            .query_map(params![id, o.turns as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        turns.sort_by_key(|t| t.0);
        let turns = turns
            .into_iter()
            .map(|(turn, prompt)| {
                let a = final_answer(conn, &id, turn)?;
                Ok(Turn { prompt, answer: a })
            })
            .collect::<Result<Vec<_>>>()?;
        let edited: Vec<String> = edited
            .query_map(params![id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        // A session that ended on an error is likely unfinished work.
        let last_error = last_event
            .query_row(params![id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()?
            .filter(|(k, _, _)| k == "error")
            .map(|(_, label, t)| format!("{}: {}", label.unwrap_or_default(), first_line(&t)));
        out.push(SessionView {
            agent,
            title,
            last: last.unwrap_or(0),
            turns,
            edited,
            last_error,
        });
    }
    Ok(out)
}

/// The turn's final answer: the last main-thread assistant text in that turn. Earlier
/// texts in the same turn are preambles ("Let me check...").
pub fn final_answer(conn: &Connection, session: &str, turn: i64) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT text FROM events
             WHERE session_id = ?1 AND turn = ?2 AND kind = 'assistant' AND thread IS NULL
             ORDER BY id DESC LIMIT 1",
        )?
        .query_row(params![session, turn], |r| r.get::<_, String>(0))
        .optional()?)
}

type Summary = (String, String);

struct Obs {
    id: i64,
    kind: String,
    title: String,
    at: i64,
}

fn memories(conn: &Connection, project: &str, limit: usize) -> Result<(Vec<Obs>, Option<Summary>)> {
    let mut s = conn.prepare(
        "SELECT id, coalesce(type, kind), coalesce(title, ''), coalesce(created_at, 0) FROM memories
         WHERE project = ?1 AND kind = 'observation' ORDER BY created_at DESC LIMIT ?2",
    )?;
    let obs = s
        .query_map(params![project, limit as i64], |r| {
            Ok(Obs {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let summary = conn
        .query_row(
            "SELECT coalesce(title, ''), coalesce(narrative, '') FROM memories
             WHERE project = ?1 AND kind = 'summary' ORDER BY created_at DESC LIMIT 1",
            params![project],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok((obs, summary))
}

fn render(
    o: &Options,
    sessions: &[SessionView],
    turns: usize,
    obs: &[Obs],
    summary: Option<&Summary>,
) -> Result<String> {
    let now = db::now_ms();
    let mut w = String::new();
    writeln!(w, "# mnem memory · {}", o.project)?;
    if !sessions.is_empty() {
        writeln!(w, "\n## Recent sessions (newest first, all agents)")?;
    }
    for (i, s) in sessions.iter().enumerate() {
        let title = s
            .title
            .as_deref()
            .map(|t| format!(" · {}", one_line(t, 80)))
            .unwrap_or_default();
        writeln!(w, "- {} · {} ago{title}", s.agent, ago(now - s.last))?;
        // The newest session is most likely what the user is continuing.
        let answer_len = if i == 0 { 600 } else { 240 };
        for t in s.turns.iter().rev().take(turns).rev() {
            writeln!(w, "  > {}", excerpt(&squash(&t.prompt), 200))?;
            if let Some(a) = &t.answer {
                writeln!(w, "  = {}", excerpt(&squash(a), answer_len))?;
            }
        }
        if !s.edited.is_empty() {
            let files: Vec<String> = s.edited.iter().map(|p| short_path(p)).collect();
            writeln!(w, "  edited: {}", files.join(", "))?;
        }
        if let Some(e) = &s.last_error {
            writeln!(w, "  ended on error: {}", one_line(e, 160))?;
        }
    }
    if let Some((req, narrative)) = summary
        && !(req.is_empty() && narrative.is_empty())
    {
        writeln!(w, "\n## Last summary")?;
        if !req.is_empty() {
            writeln!(w, "{}", one_line(req, 200))?;
        }
        for line in narrative.lines().take(5) {
            writeln!(w, "- {}", one_line(line, 240))?;
        }
    }
    if !obs.is_empty() {
        writeln!(w, "\n## Observations (full text: get_observations([ids]))")?;
        for m in obs {
            writeln!(
                w,
                "#{} {} · {} ago · {}",
                m.id,
                m.kind,
                ago(now - m.at),
                one_line(&m.title, 120)
            )?;
        }
    }
    Ok(w)
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` chars, ending at a sentence boundary when one falls in the second half,
/// else at a word boundary.
pub fn excerpt(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let floor = cut.len() / 2;
    let end = [". ", "! ", "? ", "; ", "\n"]
        .iter()
        .filter_map(|p| cut.rfind(p).map(|i| i + 1))
        .filter(|&i| i >= floor)
        .max()
        .or_else(|| cut.rfind(char::is_whitespace).filter(|&i| i >= floor))
        .unwrap_or(cut.len());
    format!("{}…", cut[..end].trim_end())
}

fn one_line(s: &str, max: usize) -> String {
    text::head(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn first_line(s: &str) -> &str {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

pub fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        ..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::excerpt;

    #[test]
    fn excerpts_end_on_sentences() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(
            excerpt("First part done. Second part is long and keeps going", 30),
            "First part done.…"
        );
        assert_eq!(excerpt("one two three four five six", 12), "one two…");
    }
}
