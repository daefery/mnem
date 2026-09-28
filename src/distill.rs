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
use std::collections::{BTreeMap, HashMap};
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
  "facts": ["..."], "concepts": ["..."], "files_read": ["..."], "files_modified": ["..."], "evidence": ["E123"]}],
 "summary": {"request": "...", "investigated": "...", "learned": "...", "completed": "...", "next_steps": "..."}}

type: exactly one of bugfix, feature, refactor, change, discovery, decision, security_alert,
security_note, sensitive (internal URLs, unreleased plans, personal details, client names).
concepts: any of how-it-works, why-it-exists, what-changed, problem-solution, gotcha, pattern, trade-off.
title: under 12 words, states the outcome ("Retry loop now backs off on 429"), not the activity.
facts: self-contained statements with concrete names, paths and values; no pronouns.
narrative: 2-4 sentences a teammate could act on.
evidence: the [E123] ids from the digest that support the observation (1-6 ids). Cite only ids
that appear in the digest; never invent one.
Use 0-5 observations; return an empty list when nothing durable happened. summary may be null."#;

/// Default preference order: cheap, capable models spread across providers, so one
/// exhausted account does not stop distillation.
pub const DEFAULT_CHAIN: &[&str] = &[
    "gpt-5.6-luna",
    "developer/claude-haiku-4-5-20251001",
    "product/claude-haiku-4-5-20251001",
    "gpt-5.6-terra",
];
/// Never used for distillation, even as an automatic fallback.
const NOT_TEXT: &[&str] = &[
    "image",
    "embedding",
    "tts",
    "whisper",
    "review",
    "realtime",
    "audio",
];
/// Cheaper models first when falling back beyond the configured chain.
const CHEAP_MARKERS: &[&str] = &["lite", "flash", "haiku", "luna", "mini", "nano", "small"];

/// An OpenAI-compatible endpoint (CLIProxyAPI by default) with an ordered model chain.
/// A model that runs out of quota, is rate limited or unavailable is put on cooldown
/// and the next one is tried; cooldowns persist across runs in `meta`.
pub struct Llm {
    base_url: String,
    key: String,
    chain: Vec<String>,
    auto_fallback: bool,
    cooldowns: std::cell::RefCell<HashMap<String, i64>>,
}

/// How a failed call affects the model that made it.
pub enum Failure {
    /// Try the next model; cool this one down for the given milliseconds (0 = none).
    NextModel(i64, String),
    /// The endpoint itself is unusable (down, bad key); stop this run.
    Endpoint(String),
}

/// Map an HTTP status to what the chain should do next.
pub fn classify_status(code: u16) -> Failure {
    const MIN: i64 = 60_000;
    match code {
        401 => Failure::Endpoint("HTTP 401 (API key rejected)".into()),
        // Quota exhausted or rate limited: give this model a long rest.
        402 | 429 => Failure::NextModel(30 * MIN, format!("HTTP {code} (quota/rate limit)")),
        // Model missing, forbidden or disabled for this account.
        403 | 404 => Failure::NextModel(6 * 60 * MIN, format!("HTTP {code} (model unavailable)")),
        // Upstream trouble: short rest.
        _ => Failure::NextModel(5 * MIN, format!("HTTP {code}")),
    }
}

/// Whole-token match, so "gemini" does not count as "mini".
fn looks_cheap(model: &str) -> bool {
    model
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| CHEAP_MARKERS.contains(&t))
}

/// Order candidates: the chain (limited to `available` when known), then, with
/// `auto`, every other text model with cheap-looking ones first. Cooling models skipped.
pub fn order_candidates(
    chain: &[String],
    available: Option<Vec<String>>,
    auto: bool,
    cooling: &HashMap<String, i64>,
    now: i64,
) -> Vec<String> {
    let ok = |m: &String| cooling.get(m).is_none_or(|until| *until <= now);
    let mut out: Vec<String> = chain
        .iter()
        .filter(|m| available.as_ref().is_none_or(|a| a.contains(m)))
        .filter(|m| ok(m))
        .cloned()
        .collect();
    if auto && let Some(a) = available {
        let mut rest: Vec<String> = a
            .into_iter()
            .filter(|m| !chain.contains(m))
            .filter(|m| !NOT_TEXT.iter().any(|x| m.to_lowercase().contains(x)))
            .filter(|m| ok(m))
            .collect();
        rest.sort_by_key(|m| !looks_cheap(m));
        out.extend(rest);
    }
    out
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
        let mut chain: Vec<String> = c.model.iter().cloned().collect();
        match &c.models {
            Some(m) => chain.extend(m.iter().cloned()),
            None if chain.is_empty() => chain.extend(DEFAULT_CHAIN.iter().map(|s| s.to_string())),
            None => {}
        }
        chain.dedup();
        Ok(Llm {
            base_url: c
                .base_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8317/v1".into()),
            key,
            chain,
            auto_fallback: c.auto_fallback.unwrap_or(true),
            cooldowns: Default::default(),
        })
    }

    fn agent(timeout: u64) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout)))
            .build()
            .new_agent()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url.trim_end_matches('/'))
    }

    /// Models the endpoint currently serves; None if it cannot say.
    fn available(&self) -> Option<Vec<String>> {
        let mut r = Self::agent(20)
            .get(&self.url("models"))
            .header("Authorization", &format!("Bearer {}", self.key))
            .call()
            .ok()?;
        let v: Value = r.body_mut().read_json().ok()?;
        let c = &CONFIG.distill;
        Some(
            v["data"]
                .as_array()?
                .iter()
                // Blocked providers (CLIProxyAPI's owned_by, e.g. "antigravity") never serve.
                .filter(|m| {
                    let owner = m["owned_by"].as_str().unwrap_or_default();
                    !c.exclude_providers
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(owner))
                })
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect(),
        )
    }

    /// Every model in order with why it is or is not used right now.
    pub fn describe(&self) -> Vec<(String, String)> {
        let usable = self.candidates();
        let now = db::now_ms();
        let cooling = self.cooldowns.borrow();
        let mut out: Vec<(String, String)> = Vec::new();
        for m in &self.chain {
            let note = match cooling.get(m).filter(|u| **u > now) {
                Some(u) => format!("  (cooling down {} more)", crate::context::ago(*u - now)),
                None if usable.contains(m) => "  (chain)".into(),
                None => "  (not served by endpoint)".into(),
            };
            out.push((m.clone(), note));
        }
        for m in usable.iter().filter(|m| !self.chain.contains(m)) {
            out.push((m.clone(), "  (auto fallback)".into()));
        }
        out
    }

    pub fn candidates(&self) -> Vec<String> {
        let blocked = |m: &String| {
            let l = m.to_lowercase();
            CONFIG
                .distill
                .exclude_models
                .iter()
                .any(|x| !x.is_empty() && l.contains(&x.to_lowercase()))
        };
        let chain: Vec<String> = self.chain.iter().filter(|m| !blocked(m)).cloned().collect();
        let available = self
            .available()
            .map(|a| a.into_iter().filter(|m| !blocked(m)).collect());
        // Without the endpoint's model list, provider blocks cannot be checked: in that
        // case only the explicit chain is tried, never an automatic fallback.
        let auto = self.auto_fallback
            && (available.is_some() || CONFIG.distill.exclude_providers.is_empty());
        order_candidates(
            &chain,
            available,
            auto,
            &self.cooldowns.borrow(),
            db::now_ms(),
        )
    }

    /// Run the prompt on the first model that answers with valid JSON.
    /// Returns the parsed JSON and the model that produced it.
    fn complete(&self, user: &str) -> Result<(Value, String)> {
        self.ask(SYSTEM, user)
    }

    /// Any JSON task through the same model chain, fallback and cooldowns.
    pub fn ask(&self, system: &str, user: &str) -> Result<(Value, String)> {
        let mut tried = Vec::new();
        for model in self.candidates() {
            match self.call(&model, system, user) {
                Ok(v) => return Ok((v, model)),
                Err(Failure::NextModel(cool_ms, why)) => {
                    if cool_ms > 0 {
                        self.cooldowns
                            .borrow_mut()
                            .insert(model.clone(), db::now_ms() + cool_ms);
                    }
                    tried.push(format!("{model}: {why}"));
                }
                Err(Failure::Endpoint(why)) => bail!("LLM endpoint unusable: {why}"),
            }
        }
        if tried.is_empty() {
            bail!("no usable model (all configured models unavailable or cooling down)");
        }
        bail!("every model failed: {}", tried.join("; "))
    }

    fn call(&self, model: &str, system: &str, user: &str) -> std::result::Result<Value, Failure> {
        let body = json!({
            "model": model,
            "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }],
            "response_format": { "type": "json_object" },
        });
        let resp = Self::agent(180)
            .post(&self.url("chat/completions"))
            .header("Authorization", &format!("Bearer {}", self.key))
            .send_json(&body);
        let mut resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(code)) => return Err(classify_status(code)),
            Err(ureq::Error::Timeout(_)) => {
                return Err(Failure::NextModel(5 * 60_000, "timeout".into()));
            }
            Err(e) => return Err(Failure::Endpoint(format!("{e}"))),
        };
        let v: Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| Failure::NextModel(0, format!("bad response: {e}")))?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        let start = content.find('{').unwrap_or(0);
        let end = content.rfind('}').map(|i| i + 1).unwrap_or(content.len());
        serde_json::from_str(content.get(start..end).unwrap_or_default())
            .map_err(|_| Failure::NextModel(0, "no valid JSON in reply".into()))
    }

    pub fn load_cooldowns(&self, conn: &Connection) {
        let saved: Option<String> = conn
            .query_row(
                "SELECT v FROM meta WHERE k = 'distill.cooldowns'",
                [],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(m) = saved.and_then(|s| serde_json::from_str::<HashMap<String, i64>>(&s).ok()) {
            let now = db::now_ms();
            self.cooldowns
                .borrow_mut()
                .extend(m.into_iter().filter(|(_, until)| *until > now));
        }
    }

    pub fn save_cooldowns(&self, conn: &Connection) -> Result<()> {
        let now = db::now_ms();
        let live: HashMap<String, i64> = self
            .cooldowns
            .borrow()
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(m, u)| (m.clone(), *u))
            .collect();
        conn.execute(
            "INSERT INTO meta(k, v) VALUES ('distill.cooldowns', ?1)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![serde_json::to_string(&live)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod chain_tests {
    use super::*;

    #[test]
    fn chain_falls_back_to_available_cheap_models() {
        let chain: Vec<String> = ["a-luna", "b-gone", "c-hot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let available = Some(
            [
                "a-luna",
                "c-hot",
                "x-opus",
                "y-flash-lite",
                "gpt-image-2",
                "codex-auto-review",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
        let mut cooling = HashMap::new();
        cooling.insert("c-hot".to_string(), 2_000);
        let order = order_candidates(&chain, available, true, &cooling, 1_000);
        // b-gone is not served; c-hot is cooling; image/review models never used;
        // cheap-looking models come before expensive ones.
        assert_eq!(order, ["a-luna", "y-flash-lite", "x-opus"]);
        assert!(!looks_cheap("gemini-pro-agent") && looks_cheap("gemini-3.1-flash-lite"));
        let order = order_candidates(&chain, None, false, &cooling, 3_000);
        assert_eq!(
            order,
            ["a-luna", "b-gone", "c-hot"],
            "unknown availability: trust the chain"
        );
    }

    #[test]
    fn forgotten_session_is_not_recreated_by_a_late_answer() {
        let d = std::env::temp_dir().join(format!("mnem-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut conn = db::open(&d.join("m.db")).unwrap();
        conn.execute(
            "INSERT INTO sessions(id, agent, native_id, project) VALUES ('pi:s', 'pi', 's', 'p')",
            [],
        )
        .unwrap();
        crate::forget::forget(&mut conn, &[], Some("pi:s"), None).unwrap();
        let chunk = Chunk {
            from: 1,
            through: 2,
            at: 1,
            text: "[E1] x".into(),
            shown: [1].into_iter().collect(),
        };
        let reply = serde_json::json!({
            "observations": [{ "type": "bugfix", "title": "late", "evidence": ["E1"] }],
            "summary": { "request": "late summary" }
        });
        assert_eq!(
            store(&mut conn, "pi:s", "p", "m", &chunk, &reply).unwrap(),
            (0, 0)
        );
        let n: i64 = conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn citations_outside_the_digest_are_dropped() {
        let shown: std::collections::HashSet<i64> = [10, 11, 12].into_iter().collect();
        let v = serde_json::json!(["E11", "E99", "e10", 12, "E11", "junk"]);
        assert_eq!(cited_ids(&v, &shown), vec![10, 11, 12]);
        assert!(cited_ids(&serde_json::json!(null), &shown).is_empty());
    }

    #[test]
    fn quota_errors_cool_down_and_move_on() {
        assert!(matches!(classify_status(429), Failure::NextModel(ms, _) if ms >= 30 * 60_000));
        assert!(matches!(classify_status(404), Failure::NextModel(ms, _) if ms > 60 * 60_000));
        assert!(matches!(classify_status(401), Failure::Endpoint(_)));
        assert!(matches!(classify_status(503), Failure::NextModel(_, _)));
    }
}

fn expand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => db::home().join(rest).to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

/// One turn of a session as the model sees it. Every item carries the id of the
/// transcript event it came from, so the model can cite its evidence.
#[derive(Default)]
struct TurnDigest {
    prompt: Option<(i64, String)>,
    harness: bool,
    answer: Option<(i64, String)>,
    edited: Vec<(i64, String)>,
    commands: Vec<(i64, String)>,
    errors: Vec<(i64, String)>,
}

struct Chunk {
    from: i64,
    through: i64,
    at: i64,
    text: String,
    /// Event ids shown to the model; citations outside this set are rejected.
    shown: std::collections::HashSet<i64>,
}

impl Chunk {
    fn empty() -> Chunk {
        Chunk {
            from: 0,
            through: 0,
            at: 0,
            text: String::new(),
            shown: Default::default(),
        }
    }
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
                d.prompt = Some((id, text::head(&t, 600)));
            }
            "assistant" => d.answer = Some((id, t)),
            "file_edit" if !d.edited.iter().any(|(_, p)| *p == path) => d.edited.push((id, path)),
            "command" if d.commands.len() < 8 => {
                d.commands.push((id, text::head(&squash(&t), 140)))
            }
            "error" if d.errors.len() < 4 => d.errors.push((
                id,
                format!(
                    "{}: {}",
                    label.unwrap_or_default(),
                    text::head(t.lines().next().unwrap_or(""), 160)
                ),
            )),
            // Agent-written recaps stand in for a final answer when a turn has none.
            "compaction" | "recap" if d.answer.is_none() => d.answer = Some((id, t)),
            _ => {}
        }
    }
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur = Chunk::empty();
    for (turn, (first, last, ts, d)) in turns {
        let mut w = String::new();
        let mut ids: Vec<i64> = Vec::new();
        let who = if d.harness { "Orchestrator" } else { "User" };
        writeln!(w, "## Turn {turn}")?;
        if let Some((id, p)) = &d.prompt {
            writeln!(w, "[E{id}] {who}: {}", squash(p))?;
            ids.push(*id);
        }
        if let Some((id, a)) = &d.answer {
            writeln!(
                w,
                "[E{id}] Agent final answer: {}",
                text::head(&squash(a), 1500)
            )?;
            ids.push(*id);
        }
        let list = |items: &[(i64, String)], ids: &mut Vec<i64>| {
            ids.extend(items.iter().map(|(i, _)| *i));
            items
                .iter()
                .map(|(i, t)| format!("[E{i}] {t}"))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        if !d.edited.is_empty() {
            writeln!(w, "Edited: {}", list(&d.edited, &mut ids))?;
        }
        if !d.commands.is_empty() {
            writeln!(w, "Commands: {}", list(&d.commands, &mut ids))?;
        }
        if !d.errors.is_empty() {
            writeln!(w, "Errors: {}", list(&d.errors, &mut ids))?;
        }
        if !cur.text.is_empty() && cur.text.len() + w.len() > CHUNK_CHARS {
            out.push(std::mem::replace(&mut cur, Chunk::empty()));
        }
        if cur.text.is_empty() {
            cur.from = first;
        }
        cur.through = last;
        cur.at = cur.at.max(ts);
        cur.text.push_str(&text::head(&w, CHUNK_CHARS));
        // Only ids whose tag survived truncation were actually sent to the model.
        cur.shown.extend(
            ids.into_iter()
                .filter(|id| cur.text.contains(&format!("[E{id}]"))),
        );
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
    /// Models that produced output this run.
    pub models: std::collections::BTreeSet<String>,
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
    if let Some(l) = &llm {
        l.load_cooldowns(conn);
    }
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
                Ok((v, model)) => {
                    st.models.insert(model.clone());
                    let (n_obs, n_sum) = store(conn, &sid, &project, &model, &c, &v)?;
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
    if let Some(l) = &llm {
        l.save_cooldowns(conn)?;
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

/// Citations like "E123" that were actually shown to the model; anything else is dropped.
fn cited_ids(v: &Value, shown: &std::collections::HashSet<i64>) -> Vec<i64> {
    let mut ids: Vec<i64> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
        })
        .filter_map(|s| s.trim().trim_start_matches(['E', 'e']).parse().ok())
        .filter(|id| shown.contains(id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Record which events support a memory, with a hash of each event's text so a later
/// change to the event (a transcript rewrite) is visible.
fn link_evidence(conn: &Connection, memory_id: i64, events: &[i64]) -> Result<()> {
    let mut ins = conn.prepare_cached(
        "INSERT OR IGNORE INTO memory_evidence(memory_id, event_id, event_hash, relation)
         SELECT ?1, id, ?3, 'cited' FROM events WHERE id = ?2",
    )?;
    for id in events {
        let t: Option<String> = conn
            .query_row(
                "SELECT coalesce(text, '') || coalesce(path, '') FROM events WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(t) = t {
            ins.execute(params![memory_id, id, text::hash(&t)])?;
        }
    }
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
    // The session may have been forgotten while the model was answering: store nothing
    // and leave no distill state behind.
    if crate::forget::session_blocked(&tx, sid, Some(project))? {
        return Ok((0, 0));
    }
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
            if crate::forget::memory_forgotten(&tx, "mnem", &format!("{base}#{i}"))? {
                continue;
            }
            let cited = cited_ids(&o["evidence"], &c.shown);
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
            if tx.changes() > 0 {
                link_evidence(&tx, tx.last_insert_rowid(), &cited)?;
            }
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
            if !(request.is_empty() && narrative.is_empty())
                && !crate::forget::memory_forgotten(&tx, "mnem", &format!("{base}#summary"))?
            {
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
