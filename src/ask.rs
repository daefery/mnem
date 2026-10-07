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
/// Memories given for a time window, at most: a busy day holds dozens of pieces of work,
/// and an answer that drops the morning is wrong, not just short.
const WINDOW_SOURCES: usize = 40;
/// Developer prompts given as evidence, at most.
const PROMPTS: usize = 12;

const SYSTEM: &str = r##"You answer a developer's question about their own past work with coding agents,
using only the sources given: memories (notes distilled from their agent sessions, id like #123) and
events (what the developer typed in those sessions, id like E456). Some memories include "asked:", what
the developer typed in that session: the most direct evidence of why something was done.
Rules: answer in 1-8 short sentences or bullets, plain words, concrete names and values. Cite the sources
you used right after the claim they support, like [#123] or [E456]. State a reason, cause or motive only
when a source states it; never infer one from a later suggestion, follow-up or next step. Give times
as dates (each source shows its date), never only a clock time or "earlier". Keep each source's own
specifics: which system, size, version or number it names. When the question asks about a time
("yesterday", a date), use only sources dated in that window and say which projects they cover. Say "in progress" or "partly done" when the sources do not show it finished; never
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

/// Personal details and pinned facts never become answer sources.
const MEMORY_FILTER: &str = "coalesce(m.type, '') != 'sensitive' AND m.kind != 'pinned'";

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

/// The sources for `question` in `scope`: a time window's activity, or the best matches.
pub fn sources(conn: &Connection, question: &str, scope: &Scope) -> Result<Vec<Source>> {
    let mut out = match &scope.window {
        Some(w) => window_memories(conn, scope, w)?,
        None => best_memories(conn, question, scope)?,
    };
    let prompts = match &scope.window {
        Some(w) => window_prompts(conn, scope, w)?,
        None => Vec::new(),
    };
    out.extend(prompts);
    Ok(out)
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

/// What a piece of work came to, most telling first: outcomes and decisions, then what a
/// session set out to do (its summary), then what was found along the way.
const WEIGHT: &str = "CASE WHEN m.type IN ('feature', 'bugfix', 'decision') THEN 0
                           WHEN m.kind = 'summary' THEN 1
                           WHEN m.type IN ('change', 'refactor') THEN 2 ELSE 3 END";

/// Memories about what happened in `w`: distilled from events in the window, in sessions
/// that are not another agent's script. When more happened than fits, the most telling
/// kinds are kept, spread over the whole window so a busy afternoon cannot crowd out the
/// morning; they are given in the order they happened.
fn window_memories(conn: &Connection, scope: &Scope, w: &Window) -> Result<Vec<Source>> {
    let (filter, mut args) = scope_filter(scope);
    let sql = format!(
        "SELECT m.id, {WEIGHT}, {HAPPENED} FROM memories m
          WHERE {filter} AND m.origin = 'mnem'
            AND NOT EXISTS (SELECT 1 FROM scripted_sessions x WHERE x.session_id = m.session_id)
            AND {HAPPENED} >= ? AND {HAPPENED} < ?"
    );
    args.push(Box::new(w.start));
    args.push(Box::new(w.end));
    let mut st = conn.prepare(&sql)?;
    let rows: Vec<(i64, i64, i64)> = st
        .query_map(
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    memory_sources(conn, &spread(rows, WINDOW_SOURCES))
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

/// For each memory source that modified files: whether its own edited lines are still there.
pub fn add_code_state(conn: &Connection, sources: &mut [Source]) {
    for s in sources.iter_mut() {
        if let Ref::Memory(id) = s.id
            && let Ok(lines) = crate::files::staleness_lines(conn, id, 2)
            && let Some(first) = lines.first()
        {
            s.code = Some(first.clone());
        }
    }
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
    Ok(read_answer(&v, sources))
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
