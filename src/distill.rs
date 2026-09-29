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

/// A session `s` (joined to its `distill_state` as `d`) has events no memory was made
/// from yet and that were not settled as too small.
const UNDISTILLED: &str = "EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id
    AND e.id > max(coalesce(d.through, 0), coalesce(d.settled, 0)) AND e.thread IS NULL)";

pub(crate) const SYSTEM: &str = r#"You turn a digest of an AI coding session into durable memory for future sessions.
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

/// The title rule in SYSTEM, and a candidate that names what the memory is about first
/// (`mnem eval --titles` compares them).
pub(crate) const TITLE_RULE: &str = r#"title: under 12 words, states the outcome ("Retry loop now backs off on 429"), not the activity."#;
pub(crate) const TITLE_RULE_NAMED: &str = r#"title: under 12 words. Start with the specific thing it concerns (the component, file,
command, setting or decision, named as the code or the team names it), then the outcome
("distill watcher: backfill runs oldest first under a daily budget", "fetch.rs retry loop now
backs off on 429"). Never a title that would fit any project ("Bug fixed", "Change passed
validation", "New sessions are no longer lost")."#;

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
    exclude_models: Vec<String>,
    exclude_providers: Vec<String>,
    cooldowns: std::cell::RefCell<HashMap<String, i64>>,
    /// Completion requests sent so far, fallbacks included (the daily budget counts these).
    requests: std::cell::Cell<usize>,
    /// Send no request once `requests` reaches this (what is left of the daily budget).
    request_limit: std::cell::Cell<Option<usize>>,
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
        Self::from_distill(&CONFIG.distill)
    }

    /// A client for the given distillation settings (the recall gate pins its judge to
    /// the live settings this way, whatever settings the candidate runs with).
    pub fn from_distill(c: &crate::config::DistillConfig) -> Result<Llm> {
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
            exclude_models: c.exclude_models.clone(),
            exclude_providers: c.exclude_providers.clone(),
            cooldowns: Default::default(),
            requests: Default::default(),
            request_limit: Default::default(),
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
        Some(
            v["data"]
                .as_array()?
                .iter()
                // Blocked providers (CLIProxyAPI's owned_by, e.g. "antigravity") never serve.
                .filter(|m| {
                    let owner = m["owned_by"].as_str().unwrap_or_default();
                    !self
                        .exclude_providers
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

    /// Which models this client asks, in order: identifies a judge in cached results.
    pub fn identity(&self) -> String {
        format!(
            "{}{}",
            self.chain.join(","),
            if self.auto_fallback { ",+auto" } else { "" }
        )
    }

    /// This one model only, no fallback (exclusions still apply).
    pub fn only(mut self, model: &str) -> Llm {
        self.chain = vec![model.to_string()];
        self.auto_fallback = false;
        self
    }

    pub fn candidates(&self) -> Vec<String> {
        let blocked = |m: &String| {
            let l = m.to_lowercase();
            self.exclude_models
                .iter()
                .any(|x| !x.is_empty() && l.contains(&x.to_lowercase()))
        };
        let chain: Vec<String> = self.chain.iter().filter(|m| !blocked(m)).cloned().collect();
        let available = self
            .available()
            .map(|a| a.into_iter().filter(|m| !blocked(m)).collect());
        // Without the endpoint's model list, provider blocks cannot be checked: in that
        // case only the explicit chain is tried, never an automatic fallback.
        let auto = self.auto_fallback && (available.is_some() || self.exclude_providers.is_empty());
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
            if self
                .request_limit
                .get()
                .is_some_and(|l| self.requests.get() >= l)
            {
                bail!("daily request budget reached after: {}", tried.join("; "));
            }
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
        self.requests.set(self.requests.get() + 1);
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
    fn small_idle_sessions_settle_and_the_backlog_is_counted() {
        let d = std::env::temp_dir().join(format!("mnem-backlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut conn = db::open(&d.join("m.db")).unwrap();
        let now = db::now_ms();
        let hour = 3_600_000;
        for (i, (id, age, text)) in [
            ("pi:old-small", 30 * hour, "hi".to_string()),
            ("pi:new-small", hour, "hi".to_string()),
            ("pi:old-big", 156 * hour, "fix the retry loop ".repeat(30)),
            ("pi:ancient", 480 * hour, "fix the retry loop ".repeat(30)),
            ("pi:expired", 204 * hour, "fix the retry loop ".repeat(30)),
        ]
        .into_iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO sessions(id, agent, native_id, project, last_event_at) VALUES (?1, 'pi', ?1, 'p', ?2)",
                params![id, now - age],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES (?1, ?2, ?3, 'prompt', ?4)",
                params![id, format!("k{i}"), now - age, text],
            )
            .unwrap();
        }
        // Only the two small sessions are in range: no model is needed, and only the one
        // idle for over a day is settled; a second pass leaves it alone.
        let st = run(&mut conn, &Options::new("manual", 2, 10)).unwrap();
        assert_eq!((st.calls, st.skipped_small, st.settled), (0, 2, 1));
        let state = |conn: &Connection, id: &str| {
            conn.query_row(
                "SELECT coalesce(max(through), 0), coalesce(max(settled), 0) FROM distill_state WHERE session_id = ?1",
                [id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap()
        };
        assert!(state(&conn, "pi:old-small").1 > 0);
        assert_eq!(state(&conn, "pi:new-small"), (0, 0));
        let st = run(&mut conn, &Options::new("manual", 2, 10)).unwrap();
        assert_eq!((st.sessions, st.settled), (1, 0), "{st:?}");

        // Settling is not distilling: when the session resumes, its tail is read again
        // with the new work (a dry run shows the digest would start from event 0).
        assert_eq!(state(&conn, "pi:old-small").0, 0);
        conn.execute(
            "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES ('pi:old-small', 'k-resume', ?1, 'prompt', 'more')",
            [now - 10 * hour],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET last_event_at = ?1 WHERE id = 'pi:old-small'",
            [now - 10 * hour],
        )
        .unwrap();
        let st = run(
            &mut conn,
            &Options {
                session: Some("pi:old-small".into()),
                dry_run: true,
                ..Options::new("manual", 2, 10)
            },
        )
        .unwrap();
        assert_eq!((st.sessions, st.skipped_small), (1, 1));

        conn.execute(
            "INSERT INTO meta(k, v) VALUES ('distill.backfill_since', ?1)",
            [(now - 240 * hour).to_string()],
        )
        .unwrap();
        let b = backlog(&conn).unwrap();
        // new-small, the resumed old-small and old-big are pending; old-big leaves the
        // 7-day window within a day; expired left it after backfill started; ancient is
        // from before that.
        assert_eq!(
            (b.pending, b.at_risk, b.expired, b.before_backfill),
            (3, 1, 1, 1)
        );
        // Nothing was distilled in the last hour, so backfill is not keeping up.
        assert!(b.falling_behind());

        // A dry run stops where the real run would: old-big needs several digests.
        let all = run(
            &mut conn,
            &Options {
                dry_run: true,
                ..Options::new("manual", 30, 10)
            },
        )
        .unwrap();
        let capped = run(
            &mut conn,
            &Options {
                dry_run: true,
                max_calls: Some(1),
                ..Options::new("manual", 30, 10)
            },
        )
        .unwrap();
        assert!(all.would_call > 1);
        assert_eq!(capped.would_call, 1);

        // A session that resumed after it was picked is not settled.
        assert!(!settle(&conn, "pi:new-small", 1).unwrap());

        // A spent budget stops the pass before any model is needed.
        for _ in 0..3 {
            record_call(&conn, "session", None, false, 2).unwrap();
        }
        assert_eq!(requests_last_day(&conn).unwrap(), 6);
        let st = run(
            &mut conn,
            &Options {
                budget: Some(6),
                ..Options::new("backfill", 7, 10)
            },
        )
        .unwrap();
        assert_eq!((st.calls, st.errors.len()), (0, 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fallbacks_stop_at_the_request_budget() {
        // Every request fails with 503, so each model falls through to the next.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let completions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = completions.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n]).contains("chat/completions") {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = s.write_all(
                    b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let cfg: crate::config::DistillConfig = serde_json::from_value(serde_json::json!({
            "base_url": format!("http://127.0.0.1:{port}/v1"),
            "api_key_env": "PATH",
            "models": ["m1", "m2", "m3"],
            "auto_fallback": false,
        }))
        .unwrap();
        let llm = Llm::from_distill(&cfg).unwrap();
        llm.request_limit.set(Some(2));
        let err = llm.complete("x").unwrap_err().to_string();
        assert!(err.contains("budget reached"), "{err}");
        assert_eq!(llm.requests.get(), 2);
        assert_eq!(completions.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn an_edited_title_is_searched_by_its_new_words() {
        let d = std::env::temp_dir().join(format!("mnem-retitle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = db::open(&d.join("m.db")).unwrap();
        conn.execute(
            "INSERT INTO memories(id, kind, title, origin, origin_id) VALUES (1, 'observation', 'Bug fixed', 'mnem', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET title = 'fetch.rs retry loop backs off' WHERE id = 1",
            [],
        )
        .unwrap();
        let hits = |q: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!((hits("retry"), hits("bug")), (1, 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_title_rule_is_the_one_in_the_prompt() {
        assert!(SYSTEM.contains(TITLE_RULE));
        assert!(!SYSTEM.contains(TITLE_RULE_NAMED));
    }

    #[test]
    fn one_distiller_per_database() {
        let d = std::env::temp_dir().join(format!("mnem-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = db::open(&d.join("m.db")).unwrap();
        let held = Lock::acquire(&conn).unwrap();
        assert!(held.is_some());
        assert!(Lock::acquire(&conn).unwrap().is_none());
        drop(held);
        assert!(Lock::acquire(&conn).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
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

pub(crate) struct Chunk {
    pub(crate) from: i64,
    pub(crate) through: i64,
    pub(crate) at: i64,
    pub(crate) text: String,
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

pub(crate) fn chunks(conn: &Connection, session: &str, after: i64) -> Result<Vec<Chunk>> {
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
    /// Only sessions whose last event is at or after this time (ms), on top of since_days.
    pub not_before: Option<i64>,
    /// Oldest sessions first (backfill) instead of newest first.
    pub oldest_first: bool,
    /// Stop after this many model calls.
    pub max_calls: Option<usize>,
    /// Recorded with each call: session, watch, backfill or manual.
    pub source: &'static str,
    /// Dry run: print each digest.
    pub verbose: bool,
    /// Stop once all distillation in the last 24 hours has sent this many requests.
    /// Checked under the distill lock before every call.
    pub budget: Option<usize>,
}

impl Options {
    pub fn new(source: &'static str, since_days: i64, limit: usize) -> Options {
        Options {
            session: None,
            since_days,
            limit,
            dry_run: false,
            include_active: false,
            not_before: None,
            oldest_first: false,
            max_calls: None,
            source,
            budget: None,
            verbose: false,
        }
    }
}

/// A session's last chunk this small waits for more work; after this long idle it is
/// settled: no longer pending, but read again with any events added if it resumes.
const SMALL_SETTLES_AFTER_MS: i64 = 86_400_000;

pub const BACKFILL_DAYS: i64 = 7;
pub const DAILY_CALLS: usize = 300;
const DAY_MS: i64 = 86_400_000;

/// Where the watcher's backfill starts: `backfill_days` before it first ran. Sessions
/// older than that were never promised and wait for a deliberate `mnem distill`.
/// Recorded when the watcher starts, whatever budget is left, so the line never moves.
pub fn backfill_since(conn: &Connection) -> Result<i64> {
    if let Some(t) = backfill_start(conn) {
        return Ok(t);
    }
    let days = crate::config::CONFIG
        .distill
        .backfill_days
        .unwrap_or(BACKFILL_DAYS);
    conn.execute(
        "INSERT OR IGNORE INTO meta(k, v) VALUES ('distill.backfill_since', ?1)",
        [(db::now_ms() - days * DAY_MS).to_string()],
    )?;
    crate::hook::log(&format!(
        "distill: backfill covers sessions active in the last {days} days"
    ));
    backfill_start(conn).context("backfill start not recorded")
}

/// Where backfill starts, if the watcher has recorded it (read only).
pub fn backfill_start(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'distill.backfill_since'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Where distillation stands, for doctor and the session-start warning.
#[derive(Debug, Default)]
pub struct Backlog {
    /// Sessions with undistilled work that the watcher will still reach.
    pub pending: usize,
    /// Of those, the ones that leave the backfill window within a day.
    pub at_risk: usize,
    /// Undistilled sessions that aged out of the window after backfill started: lost to
    /// recall until caught up by hand.
    pub expired: usize,
    /// Undistilled sessions of the last 30 days from before backfill started.
    pub before_backfill: usize,
    /// Requests sent in the last 24 hours, by all distillation.
    pub requests_day: usize,
    /// Backfill digests that produced memories in the last hour.
    pub ok_hour: usize,
    pub budget: usize,
    pub days: i64,
    pub since: Option<i64>,
}

impl Backlog {
    /// Sessions about to leave the window that backfill will not reach in time: more than
    /// today's remaining budget, or backfill distilled nothing for an hour while they wait.
    pub fn falling_behind(&self) -> bool {
        self.at_risk > 0
            && (self.at_risk > self.budget.saturating_sub(self.requests_day) || self.ok_hour == 0)
    }
}

pub fn backlog(conn: &Connection) -> Result<Backlog> {
    let cfg = &crate::config::CONFIG.distill;
    let days = cfg.backfill_days.unwrap_or(BACKFILL_DAYS);
    let since = backfill_start(conn);
    let now = db::now_ms();
    let window = now - days * DAY_MS;
    let count = |from: i64, to: i64| -> Result<usize> {
        Ok(conn.query_row(
            &format!(
                "SELECT count(*) FROM sessions s LEFT JOIN distill_state d ON d.session_id = s.id
                  WHERE s.last_event_at >= ?1 AND s.last_event_at < ?2 AND {UNDISTILLED}"
            ),
            params![from, to],
            |r| r.get::<_, i64>(0),
        )? as usize)
    };
    // Before the watcher records a start, only its regular 2-day pass reaches sessions.
    let start = window.max(since.unwrap_or(now - 2 * DAY_MS));
    let ok_hour = conn.query_row(
        "SELECT count(*) FROM distill_calls WHERE at > ?1 AND ok AND source = 'backfill'",
        [now - 3_600_000],
        |r| r.get::<_, i64>(0),
    )? as usize;
    Ok(Backlog {
        pending: count(start, now + 1)?,
        at_risk: if since.is_some() {
            count(start, (window + DAY_MS).max(start))?
        } else {
            0
        },
        expired: match since {
            Some(s) if s < window => count(s, window)?,
            _ => 0,
        },
        before_backfill: count(now - 30 * DAY_MS, start.min(since.unwrap_or(start)))?,
        requests_day: requests_last_day(conn)?,
        ok_hour,
        budget: cfg.daily_calls.unwrap_or(DAILY_CALLS),
        days,
        since,
    })
}

/// Completion requests sent by all distillation in the last 24 hours.
pub fn requests_last_day(conn: &Connection) -> Result<usize> {
    Ok(conn.query_row(
        "SELECT coalesce(sum(requests), 0) FROM distill_calls WHERE at > ?1",
        [db::now_ms() - DAY_MS],
        |r| r.get::<_, i64>(0),
    )? as usize)
}

fn record_call(
    conn: &Connection,
    source: &str,
    model: Option<&str>,
    ok: bool,
    requests: usize,
) -> Result<()> {
    conn.execute(
        "INSERT INTO distill_calls(at, source, model, ok, requests) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![db::now_ms(), source, model, ok, requests as i64],
    )?;
    Ok(())
}

/// One watcher pass: the newest idle sessions of the last 2 days for freshness, then
/// backfill, oldest first, back to `backfill_days`, a few calls at a time and only while
/// all distillation in the last 24 hours stays under `daily_calls`. Runs on its own
/// thread and connection, so slow model calls never hold up transcript capture.
pub fn watch_pass(conn: &mut Connection) {
    let cfg = &crate::config::CONFIG.distill;
    let budget = cfg.daily_calls.unwrap_or(DAILY_CALLS);
    let mut passes = vec![Options {
        max_calls: Some(10),
        ..Options::new("watch", 2, 5)
    }];
    if budget > 0 {
        match backfill_since(conn) {
            Ok(since) => passes.push(Options {
                not_before: Some(since),
                oldest_first: true,
                max_calls: Some(3),
                budget: Some(budget),
                ..Options::new("backfill", cfg.backfill_days.unwrap_or(BACKFILL_DAYS), 10)
            }),
            Err(e) => crate::hook::log(&format!("watch distill (backfill): {e:#}")),
        }
    }
    for o in passes {
        match run(conn, &o) {
            Ok(s) if s.calls > 0 || s.settled > 0 || !s.errors.is_empty() => {
                crate::hook::log(&format!(
                    "watch distill ({}): {} calls, {} observations, {} settled, {} errors",
                    o.source,
                    s.calls,
                    s.observations,
                    s.settled,
                    s.errors.len()
                ))
            }
            Ok(_) => {}
            Err(e) => crate::hook::log(&format!("watch distill ({}): {e:#}", o.source)),
        }
    }
}

#[derive(Default, Debug)]
pub struct Stats {
    pub sessions: usize,
    pub calls: usize,
    pub observations: usize,
    pub summaries: usize,
    pub skipped_small: usize,
    /// Small final chunks of sessions idle over a day, marked done.
    pub settled: usize,
    /// Dry run: calls that would be made, and the characters they would send.
    pub would_call: usize,
    pub would_chars: usize,
    pub errors: Vec<String>,
    /// Models that produced output this run.
    pub models: std::collections::BTreeSet<String>,
}

pub fn run(conn: &mut Connection, o: &Options) -> Result<Stats> {
    let Some(_lock) = Lock::acquire(conn)? else {
        return Ok(Stats {
            errors: vec!["another distill is running".into()],
            ..Default::default()
        });
    };
    let now = db::now_ms();
    let idle_before = if o.include_active { now } else { now - 120_000 };
    let mut q = conn.prepare(
        &format!(
            "SELECT s.id, coalesce(s.project, ''), s.agent, coalesce(s.title, ''), coalesce(d.through, 0),
                    coalesce(s.last_event_at, 0)
             FROM sessions s LEFT JOIN distill_state d ON d.session_id = s.id
             WHERE (?1 IS NULL OR s.id = ?1)
               AND s.last_event_at >= ?2 AND s.last_event_at <= ?3 AND {UNDISTILLED}
             ORDER BY s.last_event_at {} LIMIT ?4",
            if o.oldest_first { "ASC" } else { "DESC" }
        ),
    )?;
    let from = (now - o.since_days * 86_400_000).max(o.not_before.unwrap_or(i64::MIN));
    let sessions: Vec<(String, String, String, String, i64, i64)> = q
        .query_map(params![o.session, from, idle_before, o.limit as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(q);
    // Loaded on the first call, so a pass with nothing to send needs no model settings.
    let mut llm: Option<Llm> = None;
    let mut st = Stats::default();
    'sessions: for (sid, project, agent, title, through, last_event) in sessions {
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
                if !o.dry_run
                    && now - last_event > SMALL_SETTLES_AFTER_MS
                    && settle(conn, &sid, c.through)?
                {
                    st.settled += 1;
                }
                break;
            }
            if o.max_calls
                .is_some_and(|m| st.calls.max(st.would_call) >= m)
            {
                break 'sessions;
            }
            let left = match o.budget {
                Some(b) => match b.saturating_sub(requests_last_day(conn)?) {
                    0 => break 'sessions,
                    n => Some(n),
                },
                None => None,
            };
            let user = digest_prompt(&project, &agent, &title, &c.text);
            if o.dry_run {
                st.would_call += 1;
                st.would_chars += user.len();
                if !o.verbose {
                    continue;
                }
                println!(
                    "--- {sid} events {}..{} ({} chars)\n{}",
                    c.from,
                    c.through,
                    c.text.len(),
                    text::head(&c.text, 600)
                );
                continue;
            }
            if llm.is_none() {
                let l = Llm::from_config()?;
                l.load_cooldowns(conn);
                llm = Some(l);
            }
            let llm = llm.as_ref().expect("loaded above");
            st.calls += 1;
            let sent = llm.requests.get();
            llm.request_limit.set(left.map(|n| sent + n));
            let result = llm.complete(&user);
            record_call(
                conn,
                o.source,
                result.as_ref().ok().map(|(_, m)| m.as_str()),
                result.is_ok(),
                llm.requests.get() - sent,
            )?;
            match result {
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

/// What the model is sent for one chunk.
pub(crate) fn digest_prompt(project: &str, agent: &str, title: &str, text: &str) -> String {
    format!("Project: {project}\nAgent: {agent}\nSession title: {title}\n\n{text}")
}

/// Take a session's too-small final chunk (events through `through`) off the backlog
/// without marking it distilled, so a resumed session still reads it. Only while the
/// session is still idle when written: one that resumed during this run is left pending.
fn settle(conn: &Connection, sid: &str, through: i64) -> Result<bool> {
    let now = db::now_ms();
    Ok(conn.execute(
        "INSERT INTO distill_state(session_id, settled, updated_at)
         SELECT ?1, ?2, ?3 FROM sessions WHERE id = ?1 AND last_event_at < ?4
         ON CONFLICT(session_id) DO UPDATE SET settled = max(settled, excluded.settled),
           updated_at = excluded.updated_at",
        params![sid, through, now, now - SMALL_SETTLES_AFTER_MS],
    )? > 0)
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
pub(crate) fn store(
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

/// One distiller per database across hooks and the watcher, held with an OS file lock
/// next to it (for the live database, the data directory). The OS drops it when the
/// process ends, so a long run is never mistaken for a stale one and a crashed one never
/// blocks the next.
struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    fn acquire(conn: &Connection) -> Result<Option<Lock>> {
        let dir = conn
            .path()
            .filter(|p| !p.is_empty())
            .and_then(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(db::data_dir);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("distill.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Lock(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// Last distillation outcome for a session, for doctor/footers.
pub fn pending(conn: &Connection) -> Result<(i64, Option<String>)> {
    let n: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM sessions s LEFT JOIN distill_state d ON d.session_id = s.id
              WHERE s.last_event_at >= ?1 AND {UNDISTILLED}"
        ),
        params![db::now_ms() - 7 * 86_400_000],
        |r| r.get(0),
    )?;
    let err: Option<String> = conn
        .query_row("SELECT error FROM distill_state WHERE error IS NOT NULL ORDER BY updated_at DESC LIMIT 1", [], |r| r.get(0))
        .optional()?;
    Ok((n, err))
}
