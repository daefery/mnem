//! Tier 1: distil captured events into typed observations and session summaries.
//!
//! Runs after capture, never in front of it. Each session keeps a high-water mark
//! (`distill_state.through`); a chunk's memories and the advanced mark commit in one
//! transaction, so a failed or interrupted call only means "retry next run".

use crate::config::CONFIG;
use crate::db;
use crate::text;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::Duration;

const TYPES: &[&str] = &[
    "bugfix",
    "feature",
    "refactor",
    "change",
    "discovery",
    "decision",
    "security_alert",
    "security_note",
    "sensitive",
];
const CHUNK_CHARS: usize = 16_000;
/// Don't spend a call on a sliver of work; it stays queued until more accumulates.
const MIN_DIGEST_CHARS: usize = 300;

const SYSTEM: &str = r#"You turn a digest of an AI coding session into durable memory for future sessions.
Record durable technical signal only: what the system now does differently, what shipped, decisions
with their rationale, and concrete findings from debugging (logs, data, code paths). Skip routine
operations, empty checks, and anything already obvious from the code.

Return JSON only:
{"observations": [{"type": "...", "title": "...", "subtitle": "...", "narrative": "...",
  "facts": ["..."], "concepts": ["..."], "files_read": ["..."], "files_modified": ["..."]}],
 "summary": {"request": "...", "investigated": "...", "learned": "...", "completed": "...", "next_steps": "..."}}

type: exactly one of bugfix, feature, refactor, change, discovery, decision, security_alert,
security_note, sensitive (internal URLs, unreleased plans, personal details, client names).
concepts: any of how-it-works, why-it-exists, what-changed, problem-solution, gotcha, pattern, trade-off.
title: under 12 words, states the outcome ("Retry loop now backs off on 429"), not the activity.
facts: self-contained statements with concrete names, paths and values; no pronouns.
narrative: 2-4 sentences a teammate could act on.
Use 0-5 observations; return an empty list when nothing durable happened. summary may be null."#;

pub struct Llm {
    base_url: String,
    model: String,
    key: String,
}

impl Llm {
    pub fn from_config() -> Result<Llm> {
        let c = &CONFIG.distill;
        let key = if let Some(env) = &c.api_key_env {
            std::env::var(env).with_context(|| format!("env {env} not set"))?
        } else if let Some(path) = &c.api_key_json {
            let path = expand(path);
            let v: Value = serde_json::from_str(
                &std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?,
            )?;
            v.get(c.api_key_field.as_deref().unwrap_or("apiKey"))
                .and_then(Value::as_str)
                .context("api key field missing")?
                .to_string()
        } else {
            bail!("configure distill.api_key_env or distill.api_key_json in ~/.mnem/config.json")
        };
        Ok(Llm {
            base_url: c
                .base_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8317/v1".into()),
            model: c.model.clone().unwrap_or_else(|| "gpt-5.6-luna".into()),
            key,
        })
    }

    fn complete(&self, user: &str) -> Result<Value> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(180)))
            .build()
            .new_agent();
        let body = json!({
            "model": self.model,
            "messages": [{ "role": "system", "content": SYSTEM }, { "role": "user", "content": user }],
            "response_format": { "type": "json_object" },
        });
        let mut resp = agent
            .post(&format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .header("Authorization", &format!("Bearer {}", self.key))
            .send_json(&body)
            .context("LLM request failed")?;
        let v: Value = resp.body_mut().read_json()?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .context("no content in LLM response")?;
        let start = content.find('{').unwrap_or(0);
        let end = content.rfind('}').map(|i| i + 1).unwrap_or(content.len());
        serde_json::from_str(&content[start..end]).context("LLM returned invalid JSON")
    }
}

fn expand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => db::home().join(rest).to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

#[derive(Default)]
struct TurnDigest {
    prompt: Option<String>,
    harness: bool,
    answer: Option<String>,
    edited: Vec<String>,
    commands: Vec<String>,
    errors: Vec<String>,
}

struct Chunk {
    from: i64,
    through: i64,
    at: i64,
    text: String,
}

fn chunks(conn: &Connection, session: &str, after: i64) -> Result<Vec<Chunk>> {
    let mut s = conn.prepare(
        "SELECT id, coalesce(turn, 0), kind, label, coalesce(path, ''), coalesce(text, ''), coalesce(ts, 0)
         FROM events WHERE session_id = ?1 AND id > ?2 AND thread IS NULL ORDER BY id",
    )?;
    let mut turns: BTreeMap<i64, (i64, i64, i64, TurnDigest)> = BTreeMap::new(); // turn -> (first id, last id, ts, digest)
    let rows = s.query_map(params![session, after], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    for row in rows {
        let (id, turn, kind, label, path, t, ts) = row?;
        let e = turns
            .entry(turn)
            .or_insert((id, id, ts, TurnDigest::default()));
        e.1 = id;
        e.2 = e.2.max(ts);
        let d = &mut e.3;
        match kind.as_str() {
            "prompt" if d.prompt.is_none() => {
                d.harness = label.is_some();
                d.prompt = Some(text::head(&t, 600));
            }
            "assistant" => d.answer = Some(t),
            "file_edit" if !d.edited.contains(&path) => d.edited.push(path),
            "command" if d.commands.len() < 8 => d.commands.push(text::head(&squash(&t), 140)),
            "error" if d.errors.len() < 4 => d.errors.push(format!(
                "{}: {}",
                label.unwrap_or_default(),
                text::head(t.lines().next().unwrap_or(""), 160)
            )),
            // Agent-written recaps stand in for a final answer when a turn has none.
            "compaction" | "recap" if d.answer.is_none() => d.answer = Some(t),
            _ => {}
        }
    }
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur = Chunk {
        from: 0,
        through: 0,
        at: 0,
        text: String::new(),
    };
    for (turn, (first, last, ts, d)) in turns {
        let mut w = String::new();
        let who = if d.harness { "Orchestrator" } else { "User" };
        writeln!(w, "## Turn {turn}")?;
        if let Some(p) = &d.prompt {
            writeln!(w, "{who}: {}", squash(p))?;
        }
        if let Some(a) = &d.answer {
            writeln!(w, "Agent final answer: {}", text::head(&squash(a), 1500))?;
        }
        if !d.edited.is_empty() {
            writeln!(w, "Edited: {}", d.edited.join(", "))?;
        }
        if !d.commands.is_empty() {
            writeln!(w, "Commands: {}", d.commands.join(" | "))?;
        }
        if !d.errors.is_empty() {
            writeln!(w, "Errors: {}", d.errors.join(" | "))?;
        }
        if !cur.text.is_empty() && cur.text.len() + w.len() > CHUNK_CHARS {
            out.push(std::mem::replace(
                &mut cur,
                Chunk {
                    from: 0,
                    through: 0,
                    at: 0,
                    text: String::new(),
                },
            ));
        }
        if cur.text.is_empty() {
            cur.from = first;
        }
        cur.through = last;
        cur.at = cur.at.max(ts);
        cur.text.push_str(&text::head(&w, CHUNK_CHARS));
    }
    if !cur.text.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct Options {
    pub session: Option<String>,
    pub since_days: i64,
    pub limit: usize,
    pub dry_run: bool,
    /// Distil sessions that are still active (the Stop hook does this per turn).
    pub include_active: bool,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub sessions: usize,
    pub calls: usize,
    pub observations: usize,
    pub summaries: usize,
    pub skipped_small: usize,
    pub errors: Vec<String>,
}

pub fn run(conn: &mut Connection, o: &Options) -> Result<Stats> {
    let Some(_lock) = Lock::acquire()? else {
        return Ok(Stats {
            errors: vec!["another distill is running".into()],
            ..Default::default()
        });
    };
    let now = db::now_ms();
    let idle_before = if o.include_active { now } else { now - 120_000 };
    let mut q = conn.prepare(
        "SELECT s.id, coalesce(s.project, ''), s.agent, coalesce(s.title, ''), coalesce(d.through, 0)
         FROM sessions s LEFT JOIN distill_state d ON d.session_id = s.id
         WHERE (?1 IS NULL OR s.id = ?1)
           AND s.last_event_at >= ?2 AND s.last_event_at <= ?3
           AND EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id AND e.id > coalesce(d.through, 0) AND e.thread IS NULL)
         ORDER BY s.last_event_at DESC LIMIT ?4",
    )?;
    let sessions: Vec<(String, String, String, String, i64)> = q
        .query_map(
            params![
                o.session,
                now - o.since_days * 86_400_000,
                idle_before,
                o.limit as i64
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    drop(q);
    let llm = if o.dry_run {
        None
    } else {
        Some(Llm::from_config()?)
    };
    let mut st = Stats::default();
    for (sid, project, agent, title, through) in sessions {
        st.sessions += 1;
        // Imported claude-mem memories already cover a session up to their timestamp.
        let covered: i64 = conn.query_row(
            "SELECT coalesce(max(created_at), 0) FROM memories WHERE session_id = ?1 AND origin = 'claude-mem'",
            params![sid],
            |r| r.get(0),
        )?;
        for c in chunks(conn, &sid, through)? {
            if c.at <= covered {
                if !o.dry_run {
                    advance(conn, &sid, c.through, None)?;
                }
                continue;
            }
            if c.text.len() < MIN_DIGEST_CHARS {
                st.skipped_small += 1;
                break;
            }
            let user = format!(
                "Project: {project}\nAgent: {agent}\nSession title: {title}\n\n{}",
                c.text
            );
            let Some(llm) = &llm else {
                println!(
                    "--- {sid} events {}..{} ({} chars)\n{}",
                    c.from,
                    c.through,
                    c.text.len(),
                    text::head(&c.text, 600)
                );
                continue;
            };
            st.calls += 1;
            match llm.complete(&user) {
                Ok(v) => {
                    let (n_obs, n_sum) = store(conn, &sid, &project, &llm.model, &c, &v)?;
                    st.observations += n_obs;
                    st.summaries += n_sum;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    advance_error(conn, &sid, &msg)?;
                    st.errors.push(format!("{sid}: {msg}"));
                    break; // retry this session next run
                }
            }
        }
    }
    Ok(st)
}

fn advance(conn: &Connection, sid: &str, through: i64, err: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO distill_state(session_id, through, updated_at, error) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO UPDATE SET through = max(through, excluded.through),
           updated_at = excluded.updated_at, error = excluded.error",
        params![sid, through, db::now_ms(), err],
    )?;
    Ok(())
}

fn advance_error(conn: &Connection, sid: &str, err: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO distill_state(session_id, through, updated_at, error) VALUES (?1, 0, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET updated_at = excluded.updated_at, error = excluded.error",
        params![sid, db::now_ms(), text::head(err, 500)],
    )?;
    Ok(())
}

fn strings(v: &Value) -> String {
    let items: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(text::redact)
                .collect()
        })
        .unwrap_or_default();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
}

fn field(v: &Value, k: &str) -> String {
    text::redact(v.get(k).and_then(Value::as_str).unwrap_or_default().trim())
}

/// Insert one chunk's memories and advance the mark, atomically.
fn store(
    conn: &mut Connection,
    sid: &str,
    project: &str,
    model: &str,
    c: &Chunk,
    v: &Value,
) -> Result<(usize, usize)> {
    let tx = conn.transaction()?;
    let base = format!("{sid}@{}-{}", c.from, c.through);
    let (mut n_obs, mut n_sum) = (0, 0);
    {
        let mut ins = tx.prepare_cached(
            "INSERT OR IGNORE INTO memories(session_id, project, kind, type, title, subtitle, narrative, facts, concepts,
                 files_read, files_modified, data, origin, origin_id, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'mnem', ?13, ?14, ?15)",
        )?;
        let observations = v
            .get("observations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for (i, o) in observations.enumerate().take(8) {
            let title = field(o, "title");
            if title.is_empty() {
                continue;
            }
            let ty = o
                .get("type")
                .and_then(Value::as_str)
                .filter(|t| TYPES.contains(t))
                .unwrap_or("discovery");
            n_obs += ins.execute(params![
                sid,
                project,
                "observation",
                ty,
                title,
                field(o, "subtitle"),
                field(o, "narrative"),
                strings(&o["facts"]),
                strings(&o["concepts"]),
                strings(&o["files_read"]),
                strings(&o["files_modified"]),
                None::<String>,
                format!("{base}#{i}"),
                model,
                c.at,
            ])?;
        }
        if let Some(s) = v.get("summary").filter(|s| s.is_object()) {
            let parts = [
                ("Investigated", "investigated"),
                ("Learned", "learned"),
                ("Completed", "completed"),
                ("Next steps", "next_steps"),
            ];
            let narrative = parts
                .iter()
                .map(|(label, k)| (label, field(s, k)))
                .filter(|(_, v)| !v.is_empty())
                .map(|(label, v)| format!("{label}: {v}"))
                .collect::<Vec<_>>()
                .join("\n");
            let request = field(s, "request");
            if !(request.is_empty() && narrative.is_empty()) {
                n_sum = ins.execute(params![
                    sid,
                    project,
                    "summary",
                    None::<String>,
                    request,
                    None::<String>,
                    narrative,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                    s.to_string(),
                    format!("{base}#summary"),
                    model,
                    c.at,
                ])?;
            }
        }
    }
    advance(&tx, sid, c.through, None)?;
    tx.commit()?;
    Ok((n_obs, n_sum))
}

/// One distiller at a time across hooks and the watcher. Stale after 10 minutes.
struct Lock(std::path::PathBuf);

impl Lock {
    fn acquire() -> Result<Option<Lock>> {
        let path = db::data_dir().join("distill.lock");
        if let Ok(m) = path.metadata()
            && m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(600))
        {
            let _ = std::fs::remove_file(&path);
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Some(Lock(path))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Last distillation outcome for a session, for doctor/footers.
pub fn pending(conn: &Connection) -> Result<(i64, Option<String>)> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sessions s LEFT JOIN distill_state d ON d.session_id = s.id
         WHERE s.last_event_at >= ?1
           AND EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id AND e.id > coalesce(d.through, 0) AND e.thread IS NULL)",
        params![db::now_ms() - 7 * 86_400_000],
        |r| r.get(0),
    )?;
    let err: Option<String> = conn
        .query_row("SELECT error FROM distill_state WHERE error IS NOT NULL ORDER BY updated_at DESC LIMIT 1", [], |r| r.get(0))
        .optional()?;
    Ok((n, err))
}
