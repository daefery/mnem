//! `mnem ask`: a question about past agent work, answered from the record with sources.
//!
//! The question's scope is resolved first: the project (or every project), the time it
//! asks about ("yesterday", "on 4 October", read by `when`), and the moment it is asked
//! as of (`before`: a replayed question never sees memories or events from after it).
//!
//! A question about a time window ("what did we do yesterday") is answered from what
//! happened in that window: the memories distilled from sessions active then, ordered by
//! when their events happened, plus the developer's own prompts from those sessions. Any
//! other question gets the memories that best match it (the same hybrid ranking as MCP
//! search) plus the prompts behind them. The model answers from those sources only,
//! citing memories as [#id] and events as [E id]; a citation of something it was not
//! shown is dropped, so every source printed was really in front of it. Each memory says
//! whether the code its session wrote is still there. Without a model, the sources are
//! the answer.

use crate::distill::Llm;
use crate::when::Window;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

/// Memories given to the model, at most.
const SOURCES: usize = 8;
/// Memories given for a time window, at most: a busy week holds over a hundred outcomes
/// in one project, and an answer that drops one is wrong, not just short. They are given
/// as their titles (an outcome's title states it), so many fit.
const WINDOW_SOURCES: usize = 120;
/// Of those, given in full (the most telling first): enough detail to say what each was.
const WINDOW_FULL: usize = 24;
/// Developer prompts given as evidence, at most.
const PROMPTS: usize = 12;

const SYSTEM: &str = r##"You answer a developer's question about their own past work with coding agents,
using only the sources given: memories (notes distilled from their agent sessions, id like #123) and
events (what the developer typed in those sessions, id like E456). Some memories include "asked:", what
the developer typed in that session: the most direct evidence of why something was done.
Rules: answer in 1-8 short sentences or bullets (a question about a time may use more, one per
project or per thing done), plain words, concrete names and values. Cite the sources
you used right after the claim they support, like [#123] or [E456]. State a reason, cause or motive only
when a source states it; never infer one from a later suggestion, follow-up or next step. Say whose
reason it is: when the developer only accepted an option (their "asked:" line is a short go-ahead such
as "ok let's do X", "B", "go"), the reason was the agent's recommendation, so say "the agent
recommended it because ...; you chose it", not "we chose it because ...". A later message from the
developer saying it is still open or undecided outweighs an earlier memory that calls it decided. Give times
as dates (each source shows its date), never only a clock time or "earlier": when something started,
was decided or shipped, say on which date. A source marked "pinned by you" is the developer's own
standing rule or fact: say it is their rule and since when. Keep each source's own
specifics: which system, size, version or number it names. When the question asks about a time
("yesterday", a date), use only sources dated in that window. Over several projects, give one
bullet per project, named, listing its main outcomes by name (those marked "(shipped)" first), so no
project's work is left out or blurred. In one project, list every distinct thing that shipped or
was done, each by name; do not merge separate things into one line. Say "in progress" or "partly done" when the sources do not show it finished; never
call work done or shipped unless a source says so. If a decision is not recorded, say it is not
recorded. If the sources do not answer the question, say so in one sentence instead of guessing; then
cite nothing. Never cite an id that was not given.
Return JSON only: {"answer": "...", "cited": ["#123", "E456"]}"##;

/// Where and when a question looks.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// One project (and its threads); None for every project.
    pub project: Option<String>,
    /// The time the question is about, when it names one.
    pub window: Option<Window>,
    /// The moment the question is asked as of (epoch ms): nothing later is a source.
    pub before: Option<i64>,
}

/// What the asker gave besides the question, to resolve its scope.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    pub project: Option<String>,
    pub all: bool,
    /// The project of the directory asked from.
    pub here: Option<String>,
    /// Every known project, for one the question names.
    pub projects: Vec<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub as_of: Option<String>,
    pub now: i64,
    pub offset_min: i64,
}

/// The scope of `question`:
/// - time: --since/--until when given, else the time the question names, resolved as of
///   --as-of (or now) in the asker's local time;
/// - project: --project or --all when given; else, for a question about a time, every
///   project ("what did we do yesterday" means all of it, and the answer names each
///   project); else the directory's project. Nothing widens on its own after that: a
///   question that finds nothing in its project says so.
pub fn resolve(question: &str, a: Asked) -> Result<Scope> {
    let ts = |s: &str, what: &str| -> Result<i64> {
        let with_offset = |s: String| {
            if s.len() == 19 {
                let o = a.offset_min;
                format!(
                    "{s}{}{:02}:{:02}",
                    if o < 0 { '-' } else { '+' },
                    o.abs() / 60,
                    o.abs() % 60
                )
            } else {
                s
            }
        };
        let full = if s.len() == 10 {
            format!("{s}T00:00:00")
        } else {
            s.to_string()
        };
        crate::text::parse_ts(&with_offset(full)).ok_or_else(|| {
            anyhow::anyhow!("{what}: expected YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS, got {s}")
        })
    };
    let before = a.as_of.as_deref().map(|s| ts(s, "--as-of")).transpose()?;
    let now = before.unwrap_or(a.now);
    let window = match (&a.since, &a.until) {
        (None, None) => crate::when::window(question, now, a.offset_min),
        (since, until) => {
            let start = since
                .as_deref()
                .map(|s| ts(s, "--since"))
                .transpose()?
                .unwrap_or(0);
            let end = match until.as_deref() {
                // A bare date includes its whole day.
                Some(u) if u.len() == 10 => ts(u, "--until")? + 86_400_000,
                Some(u) => ts(u, "--until")?,
                None => now,
            };
            anyhow::ensure!(start < end, "--since must be before --until");
            Some(Window {
                start,
                end,
                label: format!(
                    "{} to {}",
                    since.as_deref().unwrap_or("the beginning"),
                    until.as_deref().unwrap_or("now")
                ),

                offset_min: a.offset_min,
            })
        }
    };
    let project = if a.all {
        None
    } else if a.project.is_some() {
        a.project
    } else if let Some(named) = named_project(question, &a.projects) {
        Some(named)
    } else if window.is_some() {
        None
    } else {
        a.here
    };
    Ok(Scope {
        project,
        window,
        before,
    })
}

/// The project `question` names ("in mnem", "on argus"): its last path part, as a word
/// of the question, matching exactly one known project. A name two projects share, or
/// a common word ("code", "tmp"), selects nothing.
fn named_project(question: &str, projects: &[String]) -> Option<String> {
    let words: Vec<String> = question
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .map(|w| w.trim_matches('.').to_string())
        .filter(|w| w.len() >= 3)
        .collect();
    const COMMON: [&str; 6] = ["code", "tmp", "home", "src", "app", "apps"];
    let name = |p: &str| -> String {
        let base = p.split('#').next().unwrap_or(p).trim_end_matches('/');
        base.rsplit('/').next().unwrap_or(base).to_lowercase()
    };
    let mut hits: Vec<&String> = projects
        .iter()
        .filter(|p| {
            let n = name(p);
            !COMMON.contains(&n.as_str()) && !p.contains('#') && words.contains(&n)
        })
        .collect();
    hits.dedup();
    let named: std::collections::HashSet<String> = hits.iter().map(|p| name(p)).collect();
    match (hits.as_slice(), named.len()) {
        ([one], 1) => Some((*one).clone()),
        _ => None,
    }
}

/// Where a question's topic is recorded when its own project holds nothing on it: the
/// projects whose memories hold at least two of its topic words, with how many, most
/// first. Searching them is the
/// asker's choice (mnem never widens on its own); this only says where to look.
pub fn elsewhere(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<(String, i64)>> {
    let Some(q) = two_of(&topic_words(question)) else {
        return Ok(Vec::new());
    };
    let mut st = conn.prepare_cached(
        "SELECT m.project, count(*) FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
          WHERE memories_fts MATCH ?1 AND instr(m.project, '/') > 0 AND m.project != ?2
            AND (?3 = 0 OR m.created_at < ?3) AND coalesce(m.type, '') != 'sensitive'
          GROUP BY m.project HAVING count(*) >= 3 ORDER BY count(*) DESC, m.project LIMIT 5",
    )?;
    let here = scope.project.clone().unwrap_or_default();
    Ok(st
        .query_map(rusqlite::params![q, here, scope.before.unwrap_or(0)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Every `k`-element combination of `items`, in order.
fn combinations<T: Clone>(items: &[T], k: usize) -> Vec<Vec<T>> {
    if k == 0 {
        return vec![vec![]];
    }
    if items.len() < k {
        return vec![];
    }
    let mut with: Vec<Vec<T>> = combinations(&items[1..], k - 1)
        .into_iter()
        .map(|mut c| {
            c.insert(0, items[0].clone());
            c
        })
        .collect();
    with.extend(combinations(&items[1..], k));
    with
}

/// The projects a question may name, for `Asked::projects`: ones identified by a
/// repository or directory (`host/owner/repo`, `/path`). Bare names an import carried
/// over ("what", "ini", "incoming") are ordinary words, not projects to choose.
pub fn projects(conn: &Connection) -> Result<Vec<String>> {
    let mut st = conn.prepare_cached(
        "SELECT DISTINCT project FROM sessions WHERE project IS NOT NULL AND instr(project, '/') > 0",
    )?;
    Ok(st
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The scope in a line, so the reader knows what was searched.
pub fn describe(scope: &Scope) -> String {
    let mut s = format!(
        "looked in {}",
        scope
            .project
            .as_deref()
            .map_or("every project".to_string(), |p| format!("project {p}"))
    );
    if let Some(w) = &scope.window {
        s.push_str(&format!(", at {}", w.label));
    }
    if let Some(b) = scope.before {
        s.push_str(&format!(", as of {}", day(b)));
    }
    s
}

/// A source: a memory (`#id`) or an event the developer typed (`E id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Memory(i64),
    Event(i64),
}

impl Ref {
    pub fn tag(&self) -> String {
        match self {
            Ref::Memory(id) => format!("#{id}"),
            Ref::Event(id) => format!("E{id}"),
        }
    }
}

/// One source shown to the model and to the reader.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: Ref,
    pub kind: String,
    pub title: String,
    /// When what it records happened (epoch ms): its events' time when known, else when it
    /// was written.
    pub at: i64,
    pub project: String,
    /// Whether its session's edits are still in the files it modified, when it can tell.
    pub code: Option<String>,
    text: String,
}

/// Personal details never become answer sources. Pinned facts are found separately
/// (`pinned_sources`) and put first: they are the developer's own words.
const MEMORY_FILTER: &str = "coalesce(m.type, '') != 'sensitive' AND m.kind != 'pinned'";
/// Pinned facts given, at most.
const PINS: usize = 3;
/// Agent replies and prompts found by their words, given as evidence, at most.
const EVENT_MATCHES: usize = 6;
/// The developer's own prompts found by one rare word, given besides those, at most.
const RARE_PROMPTS: usize = 3;
/// Longest prompt found by one rare word, in bytes: what a developer types on a topic is
/// short (their prompts' median is about 110); longer ones are mostly relayed agent
/// reports and handoffs, which mention a word in passing.
const TYPED_PROMPT: usize = 600;
/// A word in fewer than this share of the developer's prompts names a topic on its own
/// ("agpl", "galur"): one such word shared is enough for a prompt to match.
const RARE_IN_PROMPTS: f64 = 0.01;

/// SQL condition (and its bound values) for memories in `scope`'s project, written
/// before `scope.before`.
fn scope_filter(scope: &Scope) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = MEMORY_FILTER.to_string();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = &scope.project {
        sql.push_str(" AND (m.project = ? OR m.project LIKE ? || '#%')");
        args.push(Box::new(p.clone()));
        args.push(Box::new(p.clone()));
    }
    if let Some(b) = scope.before {
        sql.push_str(" AND m.created_at < ?");
        args.push(Box::new(b));
    }
    (sql, args)
}

/// The sources for `question` in `scope`: the developer's pinned facts that bear on it,
/// then a time window's activity or the best matching memories, then what was said in
/// sessions (prompts and agent replies) that matches it, which holds what no memory kept.
pub fn sources(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<Source>> {
    let mut out = pinned_sources(conn, question, scope)?;
    match &scope.window {
        Some(w) => {
            // What was said then about the question's topic comes first ("the Done field
            // on 15 September"), then the window's activity. A reply that reports on
            // another day ("here's the update for 22 September") is about that day, not
            // the one it was written on.
            let said = |v: Vec<Source>| {
                v.into_iter()
                    .filter(|s| reports_on(&s.text, s.at).is_none_or(|day| in_window(day, w)))
            };
            out.extend(said(matching_events(conn, question, scope)?));
            out.extend(window_memories(conn, question, scope, w)?);
            out.extend(said(window_prompts(conn, scope, w)?));
        }
        None => {
            out.extend(best_memories(conn, question, scope)?);
            out.extend(matching_events(conn, question, scope)?);
            out.extend(rare_word_prompts(conn, question, scope)?);
        }
    }
    let mut seen = Vec::new();
    out.retain(|s| {
        let new = !seen.contains(&s.id);
        seen.push(s.id.clone());
        new
    });
    Ok(out)
}

/// The topic words of `question`: no stop words, numbers, short words or words that only
/// set a time ("yesterday", "september"), which would match unrelated text.
fn topic_words(question: &str) -> Vec<String> {
    crate::recall::terms(question)
        .into_iter()
        .filter(|w| {
            crate::when::window(&format!("1 {w}"), 0, 0).is_none()
                && crate::when::window(w, 0, 0).is_none()
        })
        .filter(|w| {
            !matches!(
                w.as_str(),
                "week" | "month" | "last" | "since" | "minggu" | "bulan" | "lalu" | "hari"
            )
        })
        .collect()
}

/// An FTS query for text holding at least two of `words` (one shared word is noise):
/// `("a" AND "b") OR ("a" AND "c") OR ...`. None with fewer than two words.
fn two_of(words: &[String]) -> Option<String> {
    let quoted: Vec<String> = words
        .iter()
        .take(8)
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    let pairs: Vec<String> = combinations(&quoted, 2)
        .into_iter()
        .map(|p| format!("({} AND {})", p[0], p[1]))
        .collect();
    (!pairs.is_empty()).then(|| pairs.join(" OR "))
}

/// The developer's pinned facts in scope (its project's and every project's) that share
/// at least two topic words with the question, best first.
fn pinned_sources(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<Source>> {
    let Some(q) = two_of(&topic_words(question)) else {
        return Ok(Vec::new());
    };
    let mut st = conn.prepare_cached(
        "SELECT m.id FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
          WHERE memories_fts MATCH ?1 AND m.kind = 'pinned'
            AND (?2 = '' OR m.project IN (?2, '*') OR m.project LIKE ?2 || '#%')
            AND (?3 = 0 OR m.created_at < ?3)
          ORDER BY bm25(memories_fts) LIMIT ?4",
    )?;
    let project = scope.project.clone().unwrap_or_default();
    let keep: Vec<i64> = st
        .query_map(
            rusqlite::params![q, project, scope.before.unwrap_or(0), PINS as i64],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = memory_sources(conn, &keep)?;
    for s in &mut out {
        s.kind = "pinned by you".into();
    }
    Ok(out)
}

/// Prompts and agent replies in scope whose words match the question, best first: what
/// was said in a session that no memory kept (a list the agent wrote into a ticket, the
/// reason the developer gave). Main conversations only, never another agent's script or
/// a claude-mem copy of a prompt mnem read itself.
fn matching_events(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<Source>> {
    let Some(q) = two_of(&topic_words(question)) else {
        return Ok(Vec::new());
    };
    let found = said_matching(
        conn,
        &q,
        "'prompt', 'assistant'",
        usize::MAX >> 1,
        scope,
        EVENT_MATCHES * 2,
    )?;
    with_replies(
        conn,
        close_in_meaning(conn, question, found, EVENT_MATCHES),
        scope,
    )
}

/// `found`, each of the developer's prompts followed by the agent's last reply in that
/// turn: the request says what was asked ("fill the Done field with what we shipped"),
/// the reply what came of it (the list itself), which may share no word with the question.
/// A reply written after the scope's cutoff is not added.
fn with_replies(conn: &Connection, found: Vec<Source>, scope: &Scope) -> Result<Vec<Source>> {
    let mut st = conn.prepare_cached(
        "SELECT r.id, r.ts, r.text FROM events p JOIN events r
             ON r.session_id = p.session_id AND r.turn = p.turn AND r.id > p.id
          WHERE p.id = ?1 AND p.kind = 'prompt' AND r.kind = 'assistant'
            AND r.thread IS NULL AND r.label IS NULL AND r.record_key NOT LIKE 'cm:%'
            AND length(r.text) >= 40 AND (?2 = 0 OR r.ts < ?2)
          ORDER BY r.id DESC LIMIT 1",
    )?;
    let mut out: Vec<Source> = Vec::new();
    for s in found {
        let reply = match (&s.id, s.kind.as_str()) {
            (Ref::Event(id), "you asked") => st
                .query_row(rusqlite::params![id, scope.before.unwrap_or(0)], |r| {
                    let text: String = r.get(2)?;
                    Ok(Source {
                        id: Ref::Event(r.get(0)?),
                        kind: "agent said".into(),
                        title: crate::text::head(text.trim(), 100).to_string(),
                        at: r.get(1)?,
                        project: s.project.clone(),
                        code: None,
                        text: crate::text::head(text.trim(), 1500).to_string(),
                    })
                })
                .optional()?,
            _ => None,
        };
        out.push(s);
        if let Some(r) = reply.filter(|r| !out.iter().any(|o| o.id == r.id)) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Main-conversation prompts and replies in scope matching the FTS query `q`, best
/// first, as sources, at most `max_len` bytes long. `kinds` is an SQL list ("'prompt',
/// 'assistant'").
fn said_matching(
    conn: &Connection,
    q: &str,
    kinds: &str,
    max_len: usize,
    scope: &Scope,
    limit: usize,
) -> Result<Vec<Source>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT e.id, e.kind, e.ts, coalesce(s.project, ''), e.text
           FROM events_fts JOIN events e ON e.id = events_fts.rowid JOIN sessions s ON s.id = e.session_id
          WHERE events_fts MATCH ?1 AND e.kind IN ({kinds})
            AND e.thread IS NULL AND e.label IS NULL AND e.record_key NOT LIKE 'cm:%'
            AND length(e.text) >= 40 AND length(e.text) <= ?7
            AND (?2 = '' OR s.project = ?2 OR s.project LIKE ?2 || '#%')
            AND (?3 = 0 OR e.ts < ?3) AND e.ts >= ?5 AND e.ts < ?6
            AND NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = s.id)
          ORDER BY bm25(events_fts) LIMIT ?4"
    ))?;
    let project = scope.project.clone().unwrap_or_default();
    let found = st
        .query_map(
            rusqlite::params![
                q,
                project,
                scope.before.unwrap_or(0),
                limit as i64,
                scope.window.as_ref().map_or(0, |w| w.start),
                scope.window.as_ref().map_or(i64::MAX, |w| w.end),
                max_len as i64
            ],
            |r| {
                let kind: String = r.get(1)?;
                let text: String = r.get(4)?;
                Ok(Source {
                    id: Ref::Event(r.get(0)?),
                    kind: if kind == "assistant" {
                        "agent said"
                    } else {
                        "you asked"
                    }
                    .into(),
                    title: crate::text::head(text.trim(), 100).to_string(),
                    at: r.get(2)?,
                    project: r.get(3)?,
                    code: None,
                    text: crate::text::head(text.trim(), 1500).to_string(),
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(found)
}

/// Sharing words is not sharing a topic ("go ahead with the next step" meets every reply
/// that lists next steps): keep, of `found`, at most `n` close to the question in meaning,
/// when the embedding service can say. Without it, the words alone decide.
fn close_in_meaning(
    conn: &Connection,
    question: &str,
    found: Vec<Source>,
    n: usize,
) -> Vec<Source> {
    let Some(vq) = crate::embed::query_from_service(conn, question) else {
        return found.into_iter().take(n).collect();
    };
    let min = crate::recall::search_cosine();
    found
        .into_iter()
        .filter(|s| {
            // A long reply covers many things: it counts by its passage closest to the
            // question. A passage the service cannot embed does not decide.
            let scores: Vec<f32> = passages(&s.text)
                .iter()
                .filter_map(|p| crate::embed::query_from_service(conn, p))
                .filter(|v| v.model == vq.model)
                .map(|v| cosine(&vq.vec, &v.vec))
                .collect();
            scores.is_empty() || scores.iter().any(|c| *c >= min)
        })
        .take(n)
        .collect()
}

/// The developer's own prompts in scope that share one rare topic word with the question
/// ("#4 why AGPL: not sure, still open" for "why did we choose the AGPL licence?"). The
/// two-word rule misses them: a developer writes tersely, and their word on a topic (still
/// open, not a priority) outweighs what a memory says. A word is rare when it is in fewer
/// than `RARE_IN_PROMPTS` of the prompts written before the scope's cutoff.
fn rare_word_prompts(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<Source>> {
    let before = scope.before.unwrap_or(i64::MAX);
    let count = |q: Option<&str>| -> Result<i64> {
        Ok(match q {
            Some(q) => conn.query_row(
                "SELECT count(*) FROM events_fts JOIN events e ON e.id = events_fts.rowid
                  WHERE events_fts MATCH ?1 AND e.kind = 'prompt' AND e.ts < ?2",
                rusqlite::params![q, before],
                |r| r.get(0),
            )?,
            None => conn.query_row(
                "SELECT count(*) FROM events WHERE kind = 'prompt' AND ts < ?1",
                [before],
                |r| r.get(0),
            )?,
        })
    };
    let total = count(None)?.max(1) as f64;
    let mut rare = Vec::new();
    for w in topic_words(question) {
        let q = format!("\"{}\"", w.replace('"', "\"\""));
        let n = count(Some(&q))?;
        if n > 0 && (n as f64) / total < RARE_IN_PROMPTS {
            rare.push(q);
        }
    }
    if rare.is_empty() {
        return Ok(Vec::new());
    }
    let found = said_matching(
        conn,
        &rare.join(" OR "),
        "'prompt'",
        TYPED_PROMPT,
        scope,
        RARE_PROMPTS * 2,
    )?;
    with_replies(
        conn,
        close_in_meaning(conn, question, found, RARE_PROMPTS),
        scope,
    )
}

/// Passages of `text` to compare with a question: three sentences at a time, stepping by
/// two, at most six (a reply's point is near its start, and each costs an embedding).
fn passages(text: &str) -> Vec<String> {
    let mut sentences: Vec<&str> = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?' | '\n') {
            let s = text[start..i + c.len_utf8()].trim();
            if s.len() > 20 {
                sentences.push(s);
            }
            start = i + c.len_utf8();
        }
    }
    let rest = text[start..].trim();
    if rest.len() > 20 {
        sentences.push(rest);
    }
    if sentences.len() <= 3 {
        return vec![text.to_string()];
    }
    (0..sentences.len() - 2)
        .step_by(2)
        .take(6)
        .map(|i| sentences[i..i + 3].join(" "))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// The memories that best match `question`, with what the developer asked behind each.
fn best_memories(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<Source>> {
    let (filter, _) = scope_filter(scope);
    let args = || scope_filter(scope).1;
    let vq = crate::embed::query_from_service(conn, question);
    let ranked = crate::search::rank_memories(conn, question, vq.as_ref(), &filter, &args)?;
    let ids: Vec<i64> = ranked.into_iter().take(SOURCES).map(|r| r.id).collect();
    memory_sources(conn, &ids)
}

/// The time a memory's record happened: the last event of the range it was distilled
/// from (`origin_id` = `session@from-through#n`; session ids contain '-' too, so the
/// range is read after the '@'), else when it was written.
const HAPPENED: &str = "coalesce(
    (SELECT e.ts FROM events e WHERE m.origin = 'mnem' AND instr(m.origin_id, '@') > 0
       AND e.id = CAST(substr(substr(m.origin_id, instr(m.origin_id, '@') + 1),
                              instr(substr(m.origin_id, instr(m.origin_id, '@') + 1), '-') + 1)
                       AS INTEGER)),
    m.created_at)";

/// What a piece of work came to, most telling first: what reached users (a title that
/// says it shipped, was released or merged, or names a version), then other outcomes and
/// decisions, then what a session set out to do (its summary), then what was found along
/// the way.
const WEIGHT: &str = "CASE WHEN m.kind != 'summary' AND (lower(m.title) LIKE '%shipped%'
                                OR lower(m.title) LIKE '%released%' OR lower(m.title) LIKE '%merged%'
                                OR lower(m.title) LIKE '%is live%' OR m.title GLOB '*v[0-9].[0-9]*')
                                AND lower(m.title) NOT LIKE '%locally%' AND lower(m.title) NOT LIKE '%draft%'
                                AND lower(m.title) NOT LIKE '%not merged%' AND lower(m.title) NOT LIKE '%unmerged%' THEN 0
                           WHEN m.type IN ('feature', 'bugfix', 'decision') THEN 1
                           WHEN m.kind = 'summary' THEN 2
                           WHEN m.type IN ('change', 'refactor') THEN 3 ELSE 4 END";

/// Memories about what happened in `w`: distilled from events in the window, in sessions
/// that are not another agent's script. Imported claude-mem memories count by when they
/// were written (claude-mem wrote them as the session ran: within 6 minutes of its last
/// event at the median, an hour at p90), so days before mnem distilled are answerable. When more happened than fits, the most telling
/// kinds are kept, spread over the whole window so a busy afternoon cannot crowd out the
/// morning; they are given in the order they happened.
fn window_memories(
    conn: &Connection,
    question: &str,
    scope: &Scope,
    w: &Window,
) -> Result<Vec<Source>> {
    let (filter, mut args) = scope_filter(scope);
    let sql = format!(
        "SELECT m.id, {WEIGHT}, {HAPPENED} FROM memories m
          WHERE {filter} AND m.kind != 'pinned'
            AND NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = m.session_id)
            AND {HAPPENED} >= ? AND {HAPPENED} < ?"
    );
    args.push(Box::new(w.start));
    args.push(Box::new(w.end));
    let mut st = conn.prepare(&sql.replacen("SELECT m.id,", "SELECT m.project, m.id,", 1))?;
    let mut rows: Vec<(String, (i64, i64, i64))> = st
        .query_map(
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))),
        )?
        .collect::<rusqlite::Result<_>>()?;
    // A question that names a topic ("for marcom automation") is about that work: the
    // window's memories on the topic are kept, so a busy day of other work cannot crowd
    // it out. When none match, the whole window, as for "what did we do yesterday".
    // Words that say what happened ("ship", "work") or name the project already chosen
    // are not a topic.
    let topic: Vec<String> = topic_words(question)
        .into_iter()
        .filter(|t| !WORK_WORDS.contains(&t.as_str()))
        .filter(|t| {
            scope
                .project
                .as_deref()
                .is_none_or(|p| !project_names(p).contains(t))
        })
        .collect();
    if !topic.is_empty() {
        let on: std::collections::HashSet<i64> = on_topic(conn, &topic, &rows)?;
        if !on.is_empty() {
            rows.retain(|r| on.contains(&r.1.0));
        }
    }
    let kept = share(rows.clone(), WINDOW_SOURCES);
    // The most telling of them in full; the rest by title, which is what an outcome says.
    let full: std::collections::HashSet<i64> = {
        let mut best: Vec<&(String, (i64, i64, i64))> =
            rows.iter().filter(|r| kept.contains(&r.1.0)).collect();
        best.sort_by_key(|r| (r.1.1, -r.1.2));
        best.iter().take(WINDOW_FULL).map(|r| r.1.0).collect()
    };
    let shipped: std::collections::HashSet<i64> =
        rows.iter().filter(|r| r.1.1 == 0).map(|r| r.1.0).collect();
    let mut out = memory_sources(conn, &kept)?;
    for s in &mut out {
        if let Ref::Memory(id) = s.id {
            if !full.contains(&id) {
                s.text = s.title.clone();
            }
            if shipped.contains(&id) {
                s.kind = format!("{} (shipped)", s.kind);
            }
        }
    }
    Ok(out)
}

/// Verbs and nouns of a question about work done that say nothing about which work.
const WORK_WORDS: [&str; 22] = [
    "ship", "shipped", "work", "worked", "working", "build", "built", "fix", "fixed", "change",
    "changed", "happen", "happened", "update", "updates", "progress", "recap", "daily", "today",
    "kerjakan", "ngapain", "lakukan",
];

/// The names a project goes by in a question: its last path part and its thread name.
fn project_names(project: &str) -> Vec<String> {
    let (repo, thread) = project.split_once('#').unwrap_or((project, ""));
    let mut n = vec![
        repo.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(repo)
            .to_lowercase(),
    ];
    if !thread.is_empty() {
        n.push(thread.to_lowercase());
    }
    n
}

/// Of `rows`, the memories whose title or text holds one of the topic `words` (any one:
/// the window already bounds them to a day or a week).
fn on_topic(
    conn: &Connection,
    words: &[String],
    rows: &[(String, (i64, i64, i64))],
) -> Result<std::collections::HashSet<i64>> {
    let q = words
        .iter()
        .take(8)
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let ids: std::collections::HashSet<i64> = rows.iter().map(|r| r.1.0).collect();
    let mut st =
        conn.prepare_cached("SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1")?;
    let mut out = std::collections::HashSet::new();
    for id in st.query_map([q], |r| r.get::<_, i64>(0))? {
        let id = id?;
        if ids.contains(&id) {
            out.insert(id);
        }
    }
    Ok(out)
}

/// At most `n` of `(project, (id, weight, at))`, chronological, shared fairly between
/// projects: each gets an equal part (a quiet project's unused part goes to the busy
/// ones), and within its part `spread` keeps its most telling rows over its whole span.
/// So one busy project cannot crowd another's outcomes out of a day.
fn share(rows: Vec<(String, (i64, i64, i64))>, n: usize) -> Vec<i64> {
    let mut by: std::collections::BTreeMap<String, Vec<(i64, i64, i64)>> = Default::default();
    for (p, r) in rows {
        by.entry(p).or_default().push(r);
    }
    // Smallest projects first take what they have, up to an equal part of what is left.
    let mut groups: Vec<Vec<(i64, i64, i64)>> = by.into_values().collect();
    groups.sort_by_key(Vec::len);
    let count = groups.len();
    let mut left = n;
    let mut kept: Vec<(i64, i64, i64)> = Vec::new();
    for (i, g) in groups.into_iter().enumerate() {
        let ids = spread(g.clone(), left / (count - i));
        left -= ids.len();
        kept.extend(g.into_iter().filter(|r| ids.contains(&r.0)));
    }
    kept.sort_by_key(|r| (r.2, r.0));
    kept.into_iter().map(|r| r.0).collect()
}

/// At most `n` of `(id, weight, at)`, chronological: every row when they fit; else the
/// lowest weights that fit whole, and of the next weight an even sample over time.
fn spread(mut rows: Vec<(i64, i64, i64)>, n: usize) -> Vec<i64> {
    if rows.len() > n {
        rows.sort_by_key(|r| (r.1, r.2, r.0));
        let mut kept: Vec<(i64, i64, i64)> = Vec::new();
        let mut weights: Vec<i64> = rows.iter().map(|r| r.1).collect();
        weights.dedup();
        for wt in weights {
            let tier: Vec<_> = rows.iter().filter(|r| r.1 == wt).copied().collect();
            let room = n - kept.len();
            if tier.len() <= room {
                kept.extend(tier);
            } else {
                // Evenly spaced through the tier, which is in time order.
                kept.extend((0..room).map(|i| tier[i * tier.len() / room]));
            }
            if kept.len() == n {
                break;
            }
        }
        rows = kept;
    }
    rows.sort_by_key(|r| (r.2, r.0));
    rows.into_iter().map(|r| r.0).collect()
}

/// The day a message says it reports on, when it says so near its start: "update for 22
/// September", "the update for yesterday, 23 September", "daily update, Tue 23 Sep". As
/// local midnight (epoch ms) of that day, resolved against the message's own time `at`.
fn reports_on(text: &str, at: i64) -> Option<i64> {
    let head: String = text.chars().take(240).collect::<String>().to_lowercase();
    // Words that introduce an account of work done ("update for", "✅ Done — ... (22 Sep)",
    // "shipped on"), whether an agent wrote it or the developer pasted one.
    let i = [
        "update", "recap", "summary", "report", "standup", "stand-up", "done", "shipped",
        "progress",
    ]
    .iter()
    .filter_map(|w| head.find(w))
    .min()?;
    let tail = &head[i..];
    let offset = crate::when::local_offset_min();
    // A named date wins over "yesterday" ("the update for yesterday, 23 September").
    let named = crate::when::window(&tail.replace("yesterday", ""), at, offset)
        .filter(|w| w.label.starts_with("on "));
    named
        .or_else(|| {
            crate::when::window(tail, at, offset).filter(|w| w.label.starts_with("yesterday"))
        })
        .map(|w| w.start)
}

/// Whether the local day starting at `day` overlaps `w`.
fn in_window(day: i64, w: &Window) -> bool {
    day + 86_400_000 > w.start && day < w.end
}

/// What the developer typed in `w` (main conversations, not another agent's script,
/// not a claude-mem copy of a prompt mnem read itself), newest last.
fn window_prompts(conn: &Connection, scope: &Scope, w: &Window) -> Result<Vec<Source>> {
    let end = scope.before.map_or(w.end, |b| b.min(w.end));
    let mut st = conn.prepare_cached(
        "SELECT e.id, e.ts, coalesce(s.project, ''), e.text FROM events e JOIN sessions s ON s.id = e.session_id
          WHERE e.kind = 'prompt' AND e.label IS NULL AND e.thread IS NULL
            AND e.ts >= ?1 AND e.ts < ?2 AND e.record_key NOT LIKE 'cm:%'
            AND (?3 = '' OR s.project = ?3 OR s.project LIKE ?3 || '#%')
            AND NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = s.id)
            AND length(e.text) >= 12
          ORDER BY e.ts DESC LIMIT ?4",
    )?;
    let project = scope.project.clone().unwrap_or_default();
    let mut rows: Vec<Source> = st
        .query_map(
            rusqlite::params![w.start, end, project, PROMPTS as i64],
            |r| {
                let text: String = r.get(3)?;
                Ok(Source {
                    id: Ref::Event(r.get(0)?),
                    kind: "prompt".into(),
                    title: crate::text::head(text.trim(), 100).to_string(),
                    at: r.get(1)?,
                    project: r.get(2)?,
                    code: None,
                    text: crate::text::head(text.trim(), 400).to_string(),
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    rows.reverse();
    Ok(rows)
}

/// Memory rows as sources, in the order given, with what the developer asked behind each.
fn memory_sources(conn: &Connection, ids: &[i64]) -> Result<Vec<Source>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT coalesce(m.type, m.kind), coalesce(m.title, ''), {HAPPENED}, coalesce(m.project, ''),
                coalesce(m.title, '') || ' — ' || coalesce(m.subtitle, '') || ' — ' || coalesce(m.narrative, '') || ' Facts: ' || coalesce(m.facts, '')
           FROM memories m WHERE m.id = ?1"
    ))?;
    // What the developer asked in the session behind each memory, from its cited evidence:
    // the direct record of why something was done.
    let mut asked = conn.prepare_cached(
        "SELECT e.text FROM memory_evidence v JOIN events e ON e.id = v.event_id
          WHERE v.memory_id = ?1 AND e.kind = 'prompt' AND e.label IS NULL ORDER BY e.id LIMIT 2",
    )?;
    let mut out = Vec::new();
    for &id in ids {
        let row = st
            .query_row([id], |x| {
                Ok((
                    x.get::<_, String>(0)?,
                    x.get::<_, String>(1)?,
                    x.get::<_, i64>(2)?,
                    x.get::<_, String>(3)?,
                    x.get::<_, String>(4)?,
                ))
            })
            .optional()?;
        if let Some((kind, title, at, project, mut text)) = row {
            let prompts: Vec<String> = asked
                .query_map([id], |x| x.get::<_, Option<String>>(0))?
                .filter_map(|x| x.ok().flatten())
                .map(|p| crate::text::head(p.trim(), 300).to_string())
                .collect();
            if !prompts.is_empty() {
                text.push_str(&format!(" | asked: \"{}\"", prompts.join("\" / \"")));
                // Said in the data, not left to the model to notice: a choice is not a
                // reason, so any reason in this memory is the agent's.
                if prompts.iter().all(|p| is_go_ahead(p)) {
                    text.push_str(
                        " | the developer only chose or approved here; any reason above is the agent's recommendation",
                    );
                }
            }
            out.push(Source {
                id: Ref::Memory(id),
                kind,
                title,
                at,
                project,
                code: None,
                text: crate::text::head(&text, 1600).to_string(),
            });
        }
    }
    Ok(out)
}

/// `question` answered as one text, for tools (MCP, pi): the scope searched, the answer
/// with its citations when a model is configured (else none), and the sources with ids.
/// Where else the topic is recorded is said when one project held nothing on it.
pub fn answer_text(conn: &Connection, question: &str, a: Asked) -> Result<String> {
    let scope = resolve(question, a)?;
    let mut found = sources(conn, question, &scope)?;
    let mut w = format!("({})\n\n", describe(&scope));
    let elsewhere = match &scope.project {
        Some(_) => elsewhere(conn, question, &scope)?,
        None => Vec::new(),
    };
    let pointer = |w: &mut String| {
        if !elsewhere.is_empty() {
            w.push_str(
                "\nAlso recorded in other projects (ask again with project set to one of them):\n",
            );
            for (p, n) in &elsewhere {
                w.push_str(&format!("  {p} ({n} memories)\n"));
            }
        }
    };
    if found.is_empty() {
        w.push_str("Nothing recorded for that.\n");
        pointer(&mut w);
        return Ok(w);
    }
    add_code_state(conn, &mut found);
    let cited = match crate::distill::not_configured(&crate::config::CONFIG.distill) {
        Some(_) => Vec::new(),
        None => {
            let llm = Llm::from_config()?;
            llm.load_cooldowns(conn);
            let r = answer(&llm, question, &scope, &found);
            let _ = llm.save_cooldowns(conn);
            match r {
                Ok((text, cited)) => {
                    w.push_str(&text);
                    w.push_str("\n\n");
                    cited
                }
                Err(e) => {
                    w.push_str(&format!("(no answer: {e:#}; the sources are below)\n\n"));
                    Vec::new()
                }
            }
        }
    };
    w.push_str("Sources (* cited; full text: get_observations with these ids):\n");
    for s in found.iter().take(40) {
        w.push_str(&format!(
            "{} {} {} · {} · {}{}\n",
            if cited.contains(&s.id) { "*" } else { " " },
            s.id.tag(),
            s.kind,
            day(s.at),
            if scope.project.is_none() {
                format!("{} · ", s.project)
            } else {
                String::new()
            },
            crate::text::head(&s.title, 100)
        ));
        if let Some(code) = &s.code {
            w.push_str(&format!("      {code}\n"));
        }
    }
    if found.len() > 40 {
        w.push_str(&format!("  +{} more\n", found.len() - 40));
    }
    if cited.is_empty() {
        pointer(&mut w);
    }
    Ok(w)
}

/// Memory sources whose code state is checked, at most: each check reads git, and a
/// window's long tail of titles does not need it.
const CODE_STATE: usize = 12;

/// For the first memory sources that modified files: whether their own edited lines are
/// still there.
pub fn add_code_state(conn: &Connection, sources: &mut [Source]) {
    for s in sources
        .iter_mut()
        .filter(|s| matches!(s.id, Ref::Memory(_)))
        .take(CODE_STATE)
    {
        if let Ref::Memory(id) = s.id
            && let Ok(lines) = crate::files::staleness_lines(conn, id, 2)
            && let Some(first) = lines.first()
        {
            s.code = Some(first.clone());
        }
    }
}

/// Whether a prompt only picks or approves ("B", "A please", "ok let's do AGPL", "go",
/// "yes do it") and gives no reason of its own.
fn is_go_ahead(prompt: &str) -> bool {
    let p = prompt.trim().to_lowercase();
    if p.chars().count() > 60 || p.contains('?') {
        return false;
    }
    const REASON: [&str; 10] = [
        "because", "since", "so that", "so we", "karena", "supaya", "biar", "to avoid", "reason",
        "why",
    ];
    if REASON.iter().any(|r| p.contains(r)) {
        return false;
    }
    let words: Vec<&str> = p
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    const ACCEPT: [&str; 22] = [
        "ok", "okay", "yes", "yep", "sure", "go", "ahead", "do", "it", "let's", "lets", "with",
        "option", "please", "that", "use", "pick", "agree", "approved", "approve", "ya", "lanjut",
    ];
    // Options by letter or number, and at most two other words ("ok let's do AGPL").
    let other = words
        .iter()
        .filter(|w| {
            !ACCEPT.contains(w) && !(w.len() == 1 && w.chars().all(|c| c.is_ascii_alphanumeric()))
        })
        .count();
    !words.is_empty() && other <= 2
}

/// The model's answer and the sources it cited that were among `sources`.
pub fn answer(
    llm: &Llm,
    question: &str,
    scope: &Scope,
    sources: &[Source],
) -> Result<(String, Vec<Ref>)> {
    let shown: Vec<String> = sources
        .iter()
        .map(|s| {
            format!(
                "[{}] ({}, {}, {}) {}",
                s.id.tag(),
                s.kind,
                day(s.at),
                s.project,
                s.text
            )
        })
        .collect();
    let mut asked = format!("Question: {question}\n");
    if let Some(w) = &scope.window {
        asked.push_str(&format!("The question is about: {}\n", w.label));
    }
    asked.push_str(&format!(
        "Projects searched: {}\n",
        scope.project.as_deref().unwrap_or("all")
    ));
    let user = format!("{asked}\nSources:\n{}", shown.join("\n"));
    let (v, _model) = llm.ask(SYSTEM, &user)?;
    let (text, cited) = read_answer(&v, sources);
    // What was searched leads a time question's answer, in code, not left to the model:
    // "yesterday" means a date, and "what we did" means the projects it covers.
    Ok(match &scope.window {
        Some(_) => (format!("{}:\n{text}", headline(scope)), cited),
        None => (text, cited),
    })
}

/// The scope of a time question as the answer's first line: "Yesterday, 5 October 2026,
/// across all projects".
fn headline(scope: &Scope) -> String {
    let Some(w) = &scope.window else {
        return String::new();
    };
    let span = |ms: i64| long_day(ms, w.offset_min);
    let last = (w.end - 1).max(w.start);
    let when = if span(w.start) == span(last) {
        span(w.start)
    } else {
        format!("{} to {}", span(w.start), span(last))
    };
    let lead = w.label.split(" (").next().unwrap_or("");
    let lead = match lead {
        "yesterday" | "today" | "last week" | "this week" | "last month" => {
            let mut c = lead.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str() + ", ")
                .unwrap_or_default()
        }
        _ => String::new(),
    };
    let wher = match &scope.project {
        Some(p) => format!("in {p}"),
        None => "across all projects".into(),
    };
    format!("{lead}{when}, {wher}")
}

/// "5 October 2026" at UTC offset `offset_min` (the asker's, as the window was read).
fn long_day(ms: i64, offset_min: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (y, m, d) = crate::text::civil_from_days((ms + offset_min * 60_000).div_euclid(86_400_000));
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

fn day(ms: i64) -> String {
    let (y, m, d) = crate::text::civil_from_days(
        (ms + crate::when::local_offset_min() * 60_000).div_euclid(86_400_000),
    );
    format!("{y:04}-{m:02}-{d:02}")
}

/// A citation as the model may write it: "#123", "123", "E456", "[E456]".
fn parse_ref(s: &str) -> Option<Ref> {
    let s = s
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .trim_start_matches('#');
    match s.strip_prefix(['E', 'e']) {
        Some(rest) => rest.trim().parse().ok().map(Ref::Event),
        None => s.parse().ok().map(Ref::Memory),
    }
}

/// The answer text and its citations, keeping only sources that were shown; citations in
/// the text of sources that were not shown are removed too.
fn read_answer(v: &Value, sources: &[Source]) -> (String, Vec<Ref>) {
    let shown = |r: &Ref| sources.iter().any(|s| &s.id == r);
    let mut text = v["answer"].as_str().unwrap_or_default().trim().to_string();
    let mut cited: Vec<Ref> = v["cited"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| match x {
                    Value::Number(n) => n.as_i64().map(Ref::Memory),
                    Value::String(s) => parse_ref(s),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    // Citations in the text count too, if they were shown; ones not shown are removed.
    let tags = regex::Regex::new(r"\s?\[(#\d+|E\s?\d+)\]").expect("valid regex");
    text = tags
        .replace_all(&text.clone(), |c: &regex::Captures| {
            match parse_ref(&c[1]) {
                Some(r) if shown(&r) => {
                    cited.push(r);
                    c[0].to_string()
                }
                _ => String::new(),
            }
        })
        .into_owned();
    cited.retain(&shown);
    let mut seen = Vec::new();
    cited.retain(|r| {
        let new = !seen.contains(r);
        seen.push(r.clone());
        new
    });
    (text, cited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn src(id: Ref) -> Source {
        Source {
            id,
            kind: "decision".into(),
            title: String::new(),
            at: 0,
            project: "p".into(),
            code: None,
            text: String::new(),
        }
    }

    #[test]
    fn only_sources_that_were_shown_can_be_cited() {
        let shown = [
            src(Ref::Memory(10)),
            src(Ref::Memory(11)),
            src(Ref::Event(7)),
        ];
        let (text, cited) = read_answer(
            &json!({ "answer": "Backoff was added [#10], asked for [E7], then reverted [#99] [E8].", "cited": [10, "#99", "E7", 11] }),
            &shown,
        );
        assert_eq!(cited, vec![Ref::Memory(10), Ref::Event(7), Ref::Memory(11)]);
        assert_eq!(
            text,
            "Backoff was added [#10], asked for [E7], then reverted."
        );
        // Citations only in the text count when they were shown.
        let (_, cited) = read_answer(&json!({ "answer": "See [#11].", "cited": [] }), &shown);
        assert_eq!(cited, vec![Ref::Memory(11)]);
        let (text, cited) = read_answer(&json!({}), &shown);
        assert!(text.is_empty() && cited.is_empty());
    }

    fn db() -> Connection {
        crate::db::open_with(
            std::path::Path::new(":memory:"),
            std::time::Duration::from_secs(1),
        )
        .unwrap()
    }

    const H: i64 = 3_600_000;

    /// A session with one prompt and one memory distilled from it, at `at`. Session ids
    /// carry '-' like real ones ("pi:01a0f4d3-7409-..."), so the range parse is exercised.
    fn work(
        c: &Connection,
        n: i64,
        session: &str,
        project: &str,
        at: i64,
        written: i64,
        title: &str,
    ) {
        c.execute(
            "INSERT OR IGNORE INTO sessions(id, agent, native_id, project, last_event_at) VALUES (?1, 'pi', ?1, ?2, ?3)",
            rusqlite::params![session, project, at],
        )
        .unwrap();
        c.execute(
            "INSERT INTO events(id, session_id, record_key, ts, turn, kind, text) VALUES (?1, ?2, ?3, ?4, 1, 'prompt', ?5)",
            rusqlite::params![n, session, format!("k{n}"), at, format!("please {title}")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO memories(id, session_id, project, kind, type, title, origin, origin_id, created_at)
             VALUES (?1, ?2, ?3, 'summary', NULL, ?4, 'mnem', ?5, ?6)",
            rusqlite::params![n, session, project, title, format!("{session}@{n}-{n}#summary"), written],
        )
        .unwrap();
    }

    #[test]
    fn a_pinned_fact_on_the_topic_comes_first() {
        let c = db();
        work(
            &c,
            1,
            "pi:a-1",
            "p",
            10 * H,
            10 * H,
            "added the dry-run switch to publishing",
        );
        for (id, project, title) in [
            (
                50,
                "p",
                "Standing rule: every feature needs a dry-run switch for testing without going live",
            ),
            (51, "*", "Use pi in a herdr pane for council sub-agents"),
            (
                52,
                "other",
                "Every feature needs a dry-run switch in the other project too",
            ),
        ] {
            c.execute(
                "INSERT INTO memories(id, project, kind, title, origin, origin_id, created_at) VALUES (?1, ?2, 'pinned', ?3, 'user', ?1, 0)",
                rusqlite::params![id, project, title],
            )
            .unwrap();
        }
        let scope = Scope {
            project: Some("p".into()),
            window: None,
            before: None,
        };
        let got = sources(&c, "why does every feature need a dry-run switch?", &scope).unwrap();
        // Its own project's pin, first and marked; not another project's, not an unrelated one.
        assert_eq!(got[0].id, Ref::Memory(50));
        assert_eq!(got[0].kind, "pinned by you");
        assert!(!got.iter().any(|s| matches!(s.id, Ref::Memory(51 | 52))));
    }

    #[test]
    fn what_an_agent_said_is_evidence_when_no_memory_kept_it() {
        let c = db();
        work(&c, 1, "pi:a-1", "p", 10 * H, 10 * H, "marcom session");
        for (id, kind, key, text) in [
            (
                2,
                "assistant",
                "k2",
                "Marcom Done field filled: three-stage copy, audience pinned to funders, every ticket in Content Review.",
            ),
            (
                3,
                "assistant",
                "cm:assistant:3",
                "Done field filled: a claude-mem copy of the same reply about the marcom Done field.",
            ),
            (
                4,
                "command",
                "k4",
                "grep -r 'Done field' marcom/ --include=*.ts and more words here",
            ),
        ] {
            c.execute(
                "INSERT INTO events(id, session_id, record_key, ts, turn, kind, text) VALUES (?1, 'pi:a-1', ?2, ?3, 1, ?4, ?5)",
                rusqlite::params![id, key, 10 * H, kind, text],
            )
            .unwrap();
        }
        c.execute("INSERT INTO events_fts(events_fts) VALUES ('rebuild')", [])
            .unwrap();
        let scope = Scope {
            project: Some("p".into()),
            window: None,
            before: None,
        };
        let got = sources(&c, "what did we put in the Done field for marcom", &scope).unwrap();
        let events: Vec<&Source> = got
            .iter()
            .filter(|s| matches!(s.id, Ref::Event(_)))
            .collect();
        // The agent's own reply, not a claude-mem copy of it, not a shell command.
        assert_eq!(
            events.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
            [Ref::Event(2)]
        );
        assert_eq!(events[0].kind, "agent said");
        // Asked before it was said: not a source.
        let early = Scope {
            before: Some(5 * H),
            ..scope
        };
        assert!(
            sources(&c, "what did we put in the Done field for marcom", &early)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_developers_word_on_a_rare_topic_is_evidence_though_it_shares_one_word() {
        let c = db();
        work(&c, 1, "pi:a-1", "p", 10 * H, 10 * H, "adopted the licence");
        let say = |id: i64, ts: i64, text: &str| {
            c.execute(
                "INSERT INTO events(id, session_id, record_key, ts, turn, kind, text) VALUES (?1, 'pi:a-1', ?2, ?3, 1, 'prompt', ?4)",
                rusqlite::params![id, format!("k{id}"), ts, text],
            )
            .unwrap();
        };
        say(
            2,
            20 * H,
            "#4 (why AGPL): not sure, still open on different options",
        );
        // Common words are in many prompts; "agpl" only in one.
        for i in 0..200 {
            say(
                100 + i,
                11 * H,
                "please choose the next step and keep going with it",
            );
        }
        c.execute("INSERT INTO events_fts(events_fts) VALUES ('rebuild')", [])
            .unwrap();
        let scope = Scope {
            project: Some("p".into()),
            window: None,
            before: None,
        };
        let q = "why did we choose the AGPL licence?";
        let got = sources(&c, q, &scope).unwrap();
        let s = got
            .iter()
            .find(|s| s.id == Ref::Event(2))
            .expect("the prompt");
        assert_eq!(s.kind, "you asked");
        // A common word alone matches nothing.
        assert!(!got.iter().any(|s| matches!(s.id, Ref::Event(100..))));
        // Said after the replay time: not a source.
        let early = Scope {
            before: Some(15 * H),
            ..scope
        };
        assert!(
            !sources(&c, q, &early)
                .unwrap()
                .iter()
                .any(|s| s.id == Ref::Event(2))
        );
    }

    #[test]
    fn a_matched_request_brings_the_reply_that_carried_it_out() {
        let c = db();
        work(&c, 1, "pi:a-1", "p", 10 * H, 10 * H, "marcom session");
        for (id, turn, ts, kind, text) in [
            (
                2,
                7,
                11 * H,
                "prompt",
                "fill the Done field with what we already shipped for marcom automation",
            ),
            (
                3,
                7,
                11 * H + 1,
                "assistant",
                "Found the field, appending to it now, keeping what's there.",
            ),
            (
                4,
                7,
                11 * H + 2,
                "assistant",
                "Captain, written: three-stage copy, audience pinned to funders, every ticket in Content Review.",
            ),
            (
                5,
                8,
                11 * H + 3,
                "assistant",
                "Next turn: something unrelated to the request, written later on.",
            ),
        ] {
            c.execute(
                "INSERT INTO events(id, session_id, record_key, ts, turn, kind, text) VALUES (?1, 'pi:a-1', ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, format!("k{id}"), ts, turn, kind, text],
            )
            .unwrap();
        }
        c.execute("INSERT INTO events_fts(events_fts) VALUES ('rebuild')", [])
            .unwrap();
        let scope = Scope {
            project: Some("p".into()),
            window: None,
            before: None,
        };
        let q = "what did we ship for marcom automation so far?";
        let got = ids(&sources(&c, q, &scope).unwrap());
        // The turn's last reply, right after the request; not an earlier note or a later turn.
        let at = got.iter().position(|i| i == "E2").expect("the request");
        assert_eq!(got.get(at + 1).map(String::as_str), Some("E4"));
        assert!(!got.contains(&"E5".to_string()));
        // A reply written after the cutoff is not a source.
        let early = Scope {
            before: Some(11 * H + 2),
            ..scope
        };
        assert!(!ids(&sources(&c, q, &early).unwrap()).contains(&"E4".to_string()));
    }

    #[test]
    fn a_recap_is_about_the_day_it_names_not_the_day_it_was_written() {
        let ms = |s: &str| crate::text::parse_ts(s).unwrap();
        let off = crate::when::local_offset_min() * 60_000;
        let local_midnight = |s: &str| ms(&format!("{s}T00:00:00Z")) - off;
        let written = ms("2026-09-23T01:21:00Z");
        assert_eq!(
            reports_on(
                "Captain, here's the user-management update for 22 September, ready to paste",
                written
            ),
            Some(local_midnight("2026-09-22"))
        );
        assert_eq!(
            reports_on(
                "Captain, here's the update for yesterday, 23 September (Jakarta time).",
                ms("2026-09-24T01:01:00Z")
            ),
            Some(local_midnight("2026-09-23"))
        );
        assert_eq!(
            reports_on(
                "**Marcom automation: daily update, Tue 23 Sep 2026**",
                ms("2026-09-24T01:01:00Z")
            ),
            Some(local_midnight("2026-09-23"))
        );
        // A pasted update counts too, whoever typed it.
        assert_eq!(
            reports_on(
                "make the format like this:\n\n✅ Done — user management (22 Sep)\n\n- C2.8",
                written
            ),
            Some(local_midnight("2026-09-22"))
        );
        // Ordinary replies name no day they report on.
        assert_eq!(
            reports_on("The plan is in, and it carries one finding.", written),
            None
        );
        assert_eq!(
            reports_on("I will report back tomorrow with the numbers.", written),
            None
        );
    }

    #[test]
    fn a_time_answer_leads_with_what_was_searched() {
        let a = asked();
        let s = resolve("what did we do yesterday", a.clone()).unwrap();
        assert_eq!(
            headline(&s),
            "Yesterday, 5 October 2026, across all projects"
        );
        let s = resolve(
            "what did we ship in mnem last week",
            Asked {
                projects: vec!["github.com/daefery/mnem".into()],
                ..a.clone()
            },
        )
        .unwrap();
        assert_eq!(
            headline(&s),
            "Last week, 28 September 2026 to 4 October 2026, in github.com/daefery/mnem"
        );
        let s = resolve("what happened on 4 October", a.clone()).unwrap();
        assert_eq!(headline(&s), "4 October 2026, across all projects");
        assert_eq!(headline(&resolve("why did we drop it", a).unwrap()), "");
    }

    #[test]
    fn a_choice_is_not_a_reason() {
        for p in [
            "B",
            "A please",
            "ok let's do AGPL",
            "go",
            "yes do it",
            "let's go with option 2",
            "yes i think use B",
        ] {
            assert!(is_go_ahead(p), "{p}");
        }
        for p in [
            "use B because the demo must show the real flow",
            "why not A?",
            "B, karena lebih murah",
            "keep firstmate changes local, no push, no PR, no pipeline from now on",
            "close your validate-self agent if not needed anymore",
        ] {
            assert!(!is_go_ahead(p), "{p}");
        }
    }

    #[test]
    fn a_long_reply_is_compared_by_its_passages() {
        assert_eq!(passages("Short reply."), ["Short reply."]);
        let long = "First thing we did today. Second thing about the field. Third thing for the ticket. \
                    Fourth point on the audience. Fifth point about review. Sixth point about drafts.";
        let p = passages(long);
        assert_eq!(p.len(), 2);
        assert!(p[0].starts_with("First") && p[0].ends_with("ticket."));
        assert!(p[1].starts_with("Third") && p[1].ends_with("review."));
    }

    #[test]
    fn a_busy_project_cannot_crowd_another_out_of_the_day() {
        // 30 rows for "busy", 4 for "quiet", room for 10: quiet keeps all 4.
        let mut rows: Vec<(String, (i64, i64, i64))> = (0..30)
            .map(|i| ("busy".to_string(), (100 + i, 0, i)))
            .collect();
        rows.extend((0..4).map(|i| ("quiet".to_string(), (i, 0, i * 7))));
        let got = share(rows, 10);
        assert_eq!(got.len(), 10);
        assert!((0..4).all(|i| got.contains(&i)));
        // Two equally busy projects split the room evenly.
        let rows: Vec<(String, (i64, i64, i64))> = (0..20)
            .map(|i| (if i % 2 == 0 { "a" } else { "b" }.to_string(), (i, 0, i)))
            .collect();
        let got = share(rows, 6);
        assert_eq!(got.iter().filter(|i| *i % 2 == 0).count(), 3);
    }

    fn ids(s: &[Source]) -> Vec<String> {
        s.iter().map(|s| s.id.tag()).collect()
    }

    #[test]
    fn a_window_question_gets_what_happened_in_the_window_by_event_time() {
        let c = db();
        let day = 100 * 24 * H;
        work(
            &c,
            1,
            "pi:a-1-x",
            "p",
            day - 3 * H,
            day - 2 * H,
            "ship the old thing",
        ); // day before
        work(
            &c,
            2,
            "pi:b-2-x",
            "p",
            day + 9 * H,
            day + 30 * H,
            "merge the backup",
        ); // in, written next day
        work(
            &c,
            3,
            "pi:c-3-x",
            "q",
            day + 10 * H,
            day + 11 * H,
            "fix the other project",
        ); // in, other project
        work(
            &c,
            4,
            "pi:d-4-x",
            "p",
            day + 25 * H,
            day + 26 * H,
            "start tomorrow's task",
        ); // day after
        let w = Window {
            start: day,
            end: day + 24 * H,
            label: "yesterday".into(),
            offset_min: 0,
        };
        let scope = Scope {
            project: Some("p".into()),
            window: Some(w.clone()),
            before: None,
        };
        // Memory 2 counts by when its events happened, though it was written a day later.
        assert_eq!(
            ids(&sources(&c, "what did we do yesterday", &scope).unwrap()),
            ["#2", "E2"]
        );
        let every = Scope {
            project: None,
            window: Some(w),
            before: None,
        };
        let got = ids(&sources(&c, "what did we do yesterday", &every).unwrap());
        // In the order it happened, across projects.
        assert_eq!(got, ["#2", "#3", "E2", "E3"]);
    }

    #[test]
    fn a_crowded_window_keeps_its_outcomes_and_its_whole_span() {
        // 3 outcomes (weight 0) and 10 summaries (weight 1) through a day, room for 6.
        let mut rows: Vec<(i64, i64, i64)> = (0..10).map(|i| (100 + i, 1, i * 10)).collect();
        rows.extend([(1, 0, 95), (2, 0, 5), (3, 0, 50)]);
        let got = spread(rows, 6);
        // All outcomes, plus summaries from the start, middle and end, in time order.
        assert_eq!(got, [100, 2, 103, 3, 106, 1]);
        // When everything fits, everything comes, in time order.
        assert_eq!(spread(vec![(7, 3, 2), (8, 0, 1)], 6), [8, 7]);
    }

    #[test]
    fn a_topic_in_a_time_question_keeps_that_work_and_a_project_name_is_not_a_topic() {
        let c = db();
        let day = 100 * 24 * H;
        work(
            &c,
            1,
            "pi:a-1-x",
            "github.com/o/mnem",
            day + 9 * H,
            day + 9 * H,
            "marcom draft approve shipped",
        );
        work(
            &c,
            2,
            "pi:b-2-x",
            "github.com/o/mnem",
            day + 10 * H,
            day + 10 * H,
            "backup merge released",
        );
        c.execute(
            "INSERT INTO memories_fts(memories_fts) VALUES ('rebuild')",
            [],
        )
        .unwrap();
        let w = Window {
            start: day,
            end: day + 24 * H,
            label: "yesterday".into(),
            offset_min: 0,
        };
        let scope = Scope {
            project: Some("github.com/o/mnem".into()),
            window: Some(w),
            before: None,
        };
        let mems = |q: &str| -> Vec<String> {
            sources(&c, q, &scope)
                .unwrap()
                .iter()
                .filter(|s| matches!(s.id, Ref::Memory(_)))
                .map(|s| s.id.tag())
                .collect()
        };
        // "marcom" is the topic: only that work.
        assert_eq!(mems("what did we ship yesterday for marcom?"), ["#1"]);
        // The project's own name and "ship" are not topics: everything that day.
        assert_eq!(mems("what did we ship on mnem yesterday?"), ["#1", "#2"]);
        // A topic nothing matches leaves the whole day.
        assert_eq!(
            mems("what did we do yesterday about kubernetes?"),
            ["#1", "#2"]
        );
    }

    #[test]
    fn a_replayed_question_never_sees_what_was_written_after_it() {
        let c = db();
        let day = 100 * 24 * H;
        work(
            &c,
            2,
            "pi:b",
            "p",
            day + 9 * H,
            day + 30 * H,
            "merge the backup",
        );
        let w = Window {
            start: day,
            end: day + 24 * H,
            label: "yesterday".into(),
            offset_min: 0,
        };
        // Asked at day + 26h: the memory written at day + 30h did not exist yet.
        let scope = Scope {
            project: Some("p".into()),
            window: Some(w),
            before: Some(day + 26 * H),
        };
        assert_eq!(
            ids(&sources(&c, "what did we do yesterday", &scope).unwrap()),
            ["E2"]
        );
    }

    fn asked() -> Asked {
        Asked {
            here: Some("here".into()),
            now: crate::text::parse_ts("2026-10-06T02:00:00Z").unwrap(),
            offset_min: 7 * 60,
            ..Default::default()
        }
    }

    #[test]
    fn scope_follows_the_question_unless_the_asker_says_otherwise() {
        // A question about a time looks at every project, and only at that time.
        let s = resolve("what did we do yesterday", asked()).unwrap();
        assert_eq!(s.project, None);
        assert_eq!(s.window.unwrap().label, "yesterday (2026-10-05)");
        // Any other question stays in the directory's project.
        let s = resolve("why did we drop the rename", asked()).unwrap();
        assert_eq!((s.project.as_deref(), s.window), (Some("here"), None));
        // A named project and --all always win.
        let s = resolve(
            "what did we do yesterday",
            Asked {
                project: Some("p".into()),
                ..asked()
            },
        )
        .unwrap();
        assert_eq!(s.project.as_deref(), Some("p"));
        let s = resolve(
            "why did we drop it",
            Asked {
                all: true,
                ..asked()
            },
        )
        .unwrap();
        assert_eq!(s.project, None);
    }

    #[test]
    fn a_project_the_question_names_is_selected_only_when_unique() {
        let projects: Vec<String> = [
            "github.com/daefery/mnem",
            "/home/feryyp/mnem-launch",
            "github.com/fery-yp/argus",
            "github.com/kunchenguid/firstmate",
            "github.com/kunchenguid/firstmate#secondmate",
            "gitlab.x/solveearn/solveeducation",
            "github.com/other/solveeducation",
            "/home/feryyp/code",
        ]
        .map(String::from)
        .to_vec();
        let with = |q: &str| {
            resolve(
                q,
                Asked {
                    projects: projects.clone(),
                    ..asked()
                },
            )
            .unwrap()
            .project
        };
        assert_eq!(
            with("what did we ship in mnem on 4 October?").as_deref(),
            Some("github.com/daefery/mnem")
        );
        assert_eq!(
            with("in argus, why did the upload fail").as_deref(),
            Some("github.com/fery-yp/argus")
        );
        assert_eq!(
            with("what did firstmate do yesterday").as_deref(),
            Some("github.com/kunchenguid/firstmate")
        );
        // Two projects share the name: nothing is chosen for the asker.
        assert_eq!(with("what did we do yesterday in solveeducation"), None);
        // A common word is not a project name.
        assert_eq!(with("what code did we write yesterday"), None);
        // A word that merely contains a name is not it.
        assert_eq!(with("what did we do yesterday on mnemonic devices"), None);
    }

    #[test]
    fn bare_imported_names_are_not_projects_a_question_can_name() {
        let c = db();
        for (id, project) in [
            ("claude:a", "github.com/daefery/mnem"),
            ("claude:b", "what"),
            ("claude:c", "/home/me/argus"),
        ] {
            c.execute(
                "INSERT INTO sessions(id, agent, native_id, project) VALUES (?1, 'claude', ?1, ?2)",
                [id, project],
            )
            .unwrap();
        }
        let mut got = projects(&c).unwrap();
        got.sort();
        assert_eq!(got, ["/home/me/argus", "github.com/daefery/mnem"]);
    }

    #[test]
    fn as_of_moves_yesterday_and_bounds_every_source() {
        let s = resolve(
            "what did we do yesterday",
            Asked {
                as_of: Some("2026-09-24T08:20:00+07:00".into()),
                ..asked()
            },
        )
        .unwrap();
        assert_eq!(s.window.unwrap().label, "yesterday (2026-09-23)");
        assert_eq!(s.before, crate::text::parse_ts("2026-09-24T01:20:00Z"));
        // Without an offset, --as-of is local time.
        let s = resolve(
            "x",
            Asked {
                as_of: Some("2026-09-24T08:20:00".into()),
                ..asked()
            },
        )
        .unwrap();
        assert_eq!(s.before, crate::text::parse_ts("2026-09-24T01:20:00Z"));
    }

    #[test]
    fn since_and_until_replace_the_questions_time() {
        let s = resolve(
            "what did we do yesterday",
            Asked {
                since: Some("2026-09-28".into()),
                until: Some("2026-09-30".into()),
                ..asked()
            },
        )
        .unwrap();
        let w = s.window.unwrap();
        assert_eq!(
            w.start,
            crate::text::parse_ts("2026-09-28T00:00:00+07:00").unwrap()
        );
        assert_eq!(
            w.end,
            crate::text::parse_ts("2026-10-01T00:00:00+07:00").unwrap()
        );
        assert!(
            resolve(
                "x",
                Asked {
                    since: Some("soon".into()),
                    ..asked()
                }
            )
            .is_err()
        );
        assert!(
            resolve(
                "x",
                Asked {
                    since: Some("2026-10-02".into()),
                    until: Some("2026-10-01".into()),
                    ..asked()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn scripted_sessions_and_claude_mem_copies_are_not_the_developers_work() {
        let c = db();
        let day = 100 * 24 * H;
        work(&c, 1, "pi:me", "p", day + 9 * H, day + 9 * H, "my work");
        work(
            &c,
            2,
            "pi:council",
            "p",
            day + 10 * H,
            day + 10 * H,
            "council round",
        );
        crate::scripted::mark(&c, "pi:council").unwrap();
        c.execute(
            "INSERT INTO events(id, session_id, record_key, ts, turn, kind, text) VALUES (9, 'pi:me', 'cm:prompt:9', ?1, 1, 'prompt', 'please my work (copy)')",
            [day + 9 * H],
        )
        .unwrap();
        let w = Window {
            start: day,
            end: day + 24 * H,
            label: "yesterday".into(),
            offset_min: 0,
        };
        let scope = Scope {
            project: None,
            window: Some(w),
            before: None,
        };
        assert_eq!(
            ids(&sources(&c, "what did I do", &scope).unwrap()),
            ["#1", "E1"]
        );
    }
}
