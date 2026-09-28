//! Export and recall evaluation.
//!
//! `export` writes everything mnem knows as JSONL, one object per line with a `record`
//! field (session, event, memory, evidence), so data can always leave. `eval` measures prompt-time recall with a
//! known-item test: for sampled memories the model writes the question each one
//! answers (without reusing its title words), and recall must find that memory.
//! The test set lives in ~/.mnem/eval/ because it contains private work.

use crate::db;
use crate::distill::Llm;
use crate::recall;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn export(conn: &Connection, out: &mut dyn Write, project: Option<&str>) -> Result<usize> {
    let mut n = 0;
    let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    writeln!(
        out,
        "{}",
        json!({ "record": "meta", "tool": "mnem", "version": env!("CARGO_PKG_VERSION"), "schema": schema,
                "exported_at": crate::db::now_ms(), "project": project })
    )?;
    n += 1;
    let mut dump = |sql: &str, ty: &str, out: &mut dyn Write| -> Result<()> {
        let mut st = conn.prepare(sql)?;
        let cols: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = st.query(params![project])?;
        while let Some(r) = rows.next()? {
            let mut obj = serde_json::Map::new();
            // `record` names the kind of row; tables have their own `type` column.
            obj.insert("record".into(), json!(ty));
            for (i, c) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                obj.insert(
                    c.clone(),
                    match v {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => json!(i),
                        rusqlite::types::Value::Real(f) => json!(f),
                        rusqlite::types::Value::Text(t) => json!(t),
                        rusqlite::types::Value::Blob(_) => Value::Null,
                    },
                );
            }
            writeln!(out, "{}", Value::Object(obj))?;
            n += 1;
        }
        Ok(())
    };
    dump(
        "SELECT * FROM sessions WHERE ?1 IS NULL OR project = ?1 ORDER BY started_at",
        "session",
        out,
    )?;
    dump(
        "SELECT e.* FROM events e JOIN sessions s ON s.id = e.session_id WHERE ?1 IS NULL OR s.project = ?1 ORDER BY e.id",
        "event",
        out,
    )?;
    dump(
        "SELECT * FROM memories WHERE ?1 IS NULL OR project = ?1 OR project = '*' ORDER BY id",
        "memory",
        out,
    )?;
    dump(
        "SELECT v.* FROM memory_evidence v JOIN memories m ON m.id = v.memory_id WHERE ?1 IS NULL OR m.project = ?1",
        "evidence",
        out,
    )?;
    Ok(n)
}

#[derive(Serialize, Deserialize)]
struct Case {
    /// The memory that answers the question; None for a prompt no memory answers,
    /// where the right outcome is to recall nothing.
    id: Option<i64>,
    /// Further memories that equally answer it (any one counts as found).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ids: Vec<i64>,
    project: String,
    question: String,
    /// Replay the prompt as of this time (ms): memories created later are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<i64>,
    /// A real prompt whose right answer is unknown: recall is judged (`--judge`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    open: bool,
}

/// A named test set in ~/.mnem/eval: `recall` (model-written) or `vague` (hand-written
/// vague questions plus prompts that should recall nothing).
pub fn set_path(name: &str) -> PathBuf {
    db::data_dir().join("eval").join(format!("{name}.jsonl"))
}

const ASK: &str = r#"You write evaluation questions for a memory search system used by software developers.
Given one stored memory, write the question a developer would type to their coding agent weeks
later when they need this memory. Use different words from the title where you can, as a real
person would: describe the problem or goal, not the answer. One sentence, 8-25 words.
Return JSON only: {"question": "..."}"#;

/// Sample `n` distilled or imported observations across projects and have the model
/// write the question each answers.
pub fn build(conn: &Connection, n: usize, path: &Path) -> Result<usize> {
    let llm = Llm::from_config()?;
    llm.load_cooldowns(conn);
    let mut st = conn.prepare(
        "SELECT id, project, title, coalesce(subtitle, ''), coalesce(narrative, '') FROM memories
         WHERE kind = 'observation' AND length(coalesce(narrative, '')) > 120 AND project NOT LIKE '/%'
         ORDER BY abs(random()) LIMIT ?1",
    )?;
    let rows: Vec<(i64, String, String, String, String)> = st
        .query_map([n as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut out = std::fs::File::create(path)?;
    let mut written = 0;
    for (id, project, title, subtitle, narrative) in rows {
        let user = format!("Title: {title}\nSubtitle: {subtitle}\nNarrative: {narrative}");
        match llm.ask(ASK, &user) {
            Ok((v, _)) => {
                if let Some(q) = v["question"]
                    .as_str()
                    .filter(|q| q.split_whitespace().count() >= 5)
                {
                    writeln!(
                        out,
                        "{}",
                        serde_json::to_string(&Case {
                            id: Some(id),
                            ids: vec![],
                            project,
                            question: q.to_string(),
                            before: None,
                            open: false,
                        })?
                    )?;
                    written += 1;
                }
            }
            Err(e) => eprintln!("skipped #{id}: {e:#}"),
        }
    }
    llm.save_cooldowns(conn)?;
    Ok(written)
}

/// Sample `n` real human prompts from transcripts, split by a hash of their text into
/// `real-dev` (for tuning) and `real-test` (look once, never tune on it). Each is
/// replayed as of when it was typed; its answer is unknown, so recall is judged.
pub fn build_real(conn: &Connection, n: usize) -> Result<(usize, usize)> {
    let mut st = conn.prepare(
        "SELECT e.text, e.ts, s.project FROM events e JOIN sessions s ON s.id = e.session_id
         WHERE e.kind = 'prompt' AND e.label IS NULL AND e.thread IS NULL AND e.ts IS NOT NULL
           AND length(e.text) BETWEEN 40 AND 800 AND s.project NOT LIKE '/%'
           AND s.project IN (SELECT project FROM memories GROUP BY project HAVING count(*) >= 200)
         ORDER BY abs(random()) LIMIT ?1",
    )?;
    let rows: Vec<(String, i64, String)> = st
        .query_map([n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let dir = db::data_dir().join("eval");
    std::fs::create_dir_all(&dir)?;
    let mut dev = std::fs::File::create(set_path("real-dev"))?;
    let mut test = std::fs::File::create(set_path("real-test"))?;
    let (mut a, mut b) = (0, 0);
    for (question, ts, project) in rows {
        let case = serde_json::to_string(&Case {
            id: None,
            ids: vec![],
            project,
            question: question.trim().to_string(),
            before: Some(ts),
            open: true,
        })?;
        if question_key(&question).ends_with(['0', '2', '4', '6', '8', 'a', 'c', 'e']) {
            writeln!(dev, "{case}")?;
            a += 1;
        } else {
            writeln!(test, "{case}")?;
            b += 1;
        }
    }
    Ok((a, b))
}

fn question_key(q: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(q.trim().as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const JUDGE: &str = r#"You judge a memory system for coding agents. Given a developer's prompt to their
coding agent and past memories (notes from earlier sessions in the same project), mark each
memory 1 if showing it to the agent now would likely help with this prompt (same task,
component, decision, bug, rule or context), else 0. Topic overlap alone is not enough.
Return JSON only: {"relevant": [0 or 1 for each memory, in order]}"#;

/// Relevance of each memory to the prompt, judged by the distillation models and cached
/// in ~/.mnem/eval/judgments.jsonl so reruns cost nothing. None if the judge failed.
fn judge(
    conn: &Connection,
    llm: &Llm,
    cache: &mut std::collections::HashMap<(String, i64), bool>,
    project: &str,
    question: &str,
    ids: &[i64],
) -> Result<Option<Vec<bool>>> {
    let key = question_key(question);
    let todo: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| !cache.contains_key(&(key.clone(), *id)))
        .collect();
    if !todo.is_empty() {
        let mut lines = Vec::new();
        for (i, id) in todo.iter().enumerate() {
            let (t, s, n): (String, String, String) = conn.query_row(
                "SELECT coalesce(title, ''), coalesce(subtitle, ''), coalesce(narrative, '') FROM memories WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            lines.push(format!(
                "{}. {t} — {s} — {}",
                i + 1,
                crate::text::head(&n, 300)
            ));
        }
        let user = format!(
            "Project: {project}\nPrompt:\n{}\n\nMemories:\n{}",
            crate::text::head(question, 1500),
            lines.join("\n")
        );
        let Ok((v, _)) = llm.ask(JUDGE, &user) else {
            return Ok(None);
        };
        let marks: Vec<bool> = v["relevant"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_i64() == Some(1)).collect())
            .unwrap_or_default();
        if marks.len() != todo.len() {
            return Ok(None);
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(set_path("judgments"))?;
        for (id, rel) in todo.iter().zip(marks) {
            writeln!(f, "{}", json!({ "q": key, "id": id, "relevant": rel }))?;
            cache.insert((key.clone(), *id), rel);
        }
    }
    Ok(Some(
        ids.iter().map(|id| cache[&(key.clone(), *id)]).collect(),
    ))
}

fn load_judgments() -> std::collections::HashMap<(String, i64), bool> {
    let mut m = std::collections::HashMap::new();
    if let Ok(f) = std::fs::File::open(set_path("judgments")) {
        for l in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&l)
                && let (Some(q), Some(id), Some(rel)) =
                    (v["q"].as_str(), v["id"].as_i64(), v["relevant"].as_bool())
            {
                m.insert((q.to_string(), id), rel);
            }
        }
    }
    m
}

/// Recall on real prompts as judged: memories shown, how many helped, and how many
/// prompts got at least one useful memory.
#[derive(Default)]
pub struct Judged {
    pub prompts: usize,
    pub shown: usize,
    pub right: usize,
    pub helped: usize,
    pub unjudged: usize,
}

pub struct Report {
    pub cases: usize,
    /// Cases whose target is a sensitive memory (excluded from automatic recall).
    pub skipped: usize,
    pub hit1: usize,
    pub hit5: usize,
    pub mrr: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub misses: Vec<(i64, String)>,
    /// Prompts no memory answers, and how many of them still recalled something.
    pub negatives: usize,
    pub false_alarms: Vec<String>,
    /// Mean share of the top five that are targets, over cases that recalled anything.
    pub precision5: f64,
    /// Answerable cases that recalled nothing at all.
    pub silent: usize,
    pub judged: Judged,
}

pub fn run(conn: &Connection, path: &Path, mode: recall::Mode, judging: bool) -> Result<Report> {
    let llm = if judging {
        let l = Llm::from_config()?;
        l.load_cooldowns(conn);
        Some(l)
    } else {
        None
    };
    let mut cache = load_judgments();
    let mut judged = Judged::default();
    let embedder = recall::semantic_embedder();
    let f = std::fs::File::open(path).with_context(|| {
        format!(
            "no test set at {}; run `mnem eval --build 40`",
            path.display()
        )
    })?;
    let cases: Vec<Case> = std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    let (mut hit1, mut hit5, mut mrr) = (0, 0, 0.0);
    let mut times = Vec::new();
    let mut misses = Vec::new();
    let mut skipped = 0;
    let (mut negatives, mut false_alarms) = (0, Vec::new());
    let (mut precision, mut precise_n, mut silent) = (0.0, 0, 0);
    let mut created = conn.prepare("SELECT coalesce(created_at, 0) FROM memories WHERE id = ?1")?;
    for c in &cases {
        // Sensitive memories are kept out of automatic recall on purpose; not a miss.
        let sensitive: bool = c.id.is_some_and(|id| {
            conn.query_row(
                "SELECT coalesce(type, '') = 'sensitive' FROM memories WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap_or(false)
        });
        if sensitive {
            skipped += 1;
            continue;
        }
        let t = Instant::now();
        let query = embedder.as_ref().map(|e| e.query(&c.question));
        let mut ranked = recall::rank(
            conn,
            &c.project,
            &c.question,
            None,
            if c.before.is_some() { 60 } else { 10 },
            query.as_ref(),
            mode,
        )?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        // Replaying an old prompt: drop what did not exist yet. Scores are per memory,
        // so the order of the rest is the order recall would have produced then.
        if let Some(before) = c.before {
            ranked.retain(|r| {
                created
                    .query_row([r.0], |x| x.get::<_, i64>(0))
                    .is_ok_and(|t| t < before)
            });
            ranked.truncate(10);
        }
        let targets: Vec<i64> = c.id.into_iter().chain(c.ids.iter().copied()).collect();
        if c.open {
            let top: Vec<i64> = ranked.iter().take(5).map(|r| r.0).collect();
            judged.prompts += 1;
            judged.shown += top.len();
            if let Some(llm) = &llm
                && !top.is_empty()
            {
                match judge(conn, llm, &mut cache, &c.project, &c.question, &top)? {
                    Some(marks) => {
                        let right = marks.iter().filter(|m| **m).count();
                        judged.right += right;
                        judged.helped += (right > 0) as usize;
                    }
                    None => judged.unjudged += 1,
                }
            }
            continue;
        }
        let Some(&target) = targets.first() else {
            negatives += 1;
            if !ranked.is_empty() {
                false_alarms.push(c.question.clone());
            }
            continue;
        };
        let top: Vec<i64> = ranked.iter().take(5).map(|r| r.0).collect();
        if top.is_empty() {
            silent += 1;
        } else {
            precision +=
                top.iter().filter(|id| targets.contains(id)).count() as f64 / top.len() as f64;
            precise_n += 1;
        }
        match ranked.iter().position(|r| targets.contains(&r.0)) {
            Some(i) => {
                hit1 += (i == 0) as usize;
                hit5 += (i < 5) as usize;
                mrr += 1.0 / (i as f64 + 1.0);
                if i >= 5 {
                    misses.push((target, c.question.clone()));
                }
            }
            None => misses.push((target, c.question.clone())),
        }
    }
    times.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        times
            .get(((times.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    if let Some(llm) = &llm {
        llm.save_cooldowns(conn)?;
    }
    let positives = cases.len() - skipped - negatives - judged.prompts;
    Ok(Report {
        cases: positives,
        skipped,
        hit1,
        hit5,
        mrr: if positives == 0 {
            0.0
        } else {
            mrr / positives as f64
        },
        negatives,
        false_alarms,
        precision5: if precise_n == 0 {
            0.0
        } else {
            precision / precise_n as f64
        },
        silent,
        judged,
        p50_ms: pct(0.5),
        p95_ms: pct(0.95),
        misses,
    })
}

/// Cosine of each question's true target, and of the best wrong candidate, under the
/// current embedding model. Used to pick recall's similarity floor from data.
pub fn cosines(conn: &Connection, path: &Path) -> Result<(Vec<f32>, Vec<f32>)> {
    let e = recall::semantic_embedder().context("no embedding model (run `mnem embed`)")?;
    let f = std::fs::File::open(path)?;
    let cases: Vec<Case> = std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    let (mut target, mut wrong) = (Vec::new(), Vec::new());
    for c in &cases {
        let Some(id) = c.id else { continue };
        let q = e.query(&c.question);
        let hits = crate::embed::search(conn, &q, &c.project, None, 500)?;
        if let Some((_, cos)) = hits.iter().find(|h| h.0 == id) {
            target.push(*cos);
        }
        if let Some((_, cos)) = hits.iter().find(|h| h.0 != id) {
            wrong.push(*cos);
        }
    }
    target.sort_by(f32::total_cmp);
    wrong.sort_by(f32::total_cmp);
    Ok((target, wrong))
}

#[cfg(test)]
mod tests {
    use super::Case;

    #[test]
    fn test_set_lines_old_and_new() {
        let old: Case = serde_json::from_str(r#"{"id":7,"project":"p","question":"why"}"#).unwrap();
        assert_eq!(
            (old.id, old.ids.len(), old.open, old.before),
            (Some(7), 0, false, None)
        );
        let none: Case =
            serde_json::from_str(r#"{"id":null,"project":"p","question":"dinner?"}"#).unwrap();
        assert!(none.id.is_none() && !none.open);
        let real: Case = serde_json::from_str(
            r#"{"id":null,"project":"p","question":"fix it","before":5,"open":true}"#,
        )
        .unwrap();
        assert!(real.open && real.before == Some(5));
        // Written back, defaults stay out of the file.
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"id":7,"project":"p","question":"why"}"#
        );
    }
}
