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
    id: i64,
    project: String,
    question: String,
}

pub fn eval_path() -> PathBuf {
    db::data_dir().join("eval").join("recall.jsonl")
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
                            id,
                            project,
                            question: q.to_string()
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
}

pub fn run(conn: &Connection, path: &Path) -> Result<Report> {
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
    for c in &cases {
        // Sensitive memories are kept out of automatic recall on purpose; not a miss.
        let sensitive: bool = conn
            .query_row(
                "SELECT coalesce(type, '') = 'sensitive' FROM memories WHERE id = ?1",
                [c.id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if sensitive {
            skipped += 1;
            continue;
        }
        let t = Instant::now();
        let ranked = recall::rank(conn, &c.project, &c.question, None, 10)?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        match ranked.iter().position(|r| r.0 == c.id) {
            Some(i) => {
                hit1 += (i == 0) as usize;
                hit5 += (i < 5) as usize;
                mrr += 1.0 / (i as f64 + 1.0);
                if i >= 5 {
                    misses.push((c.id, c.question.clone()));
                }
            }
            None => misses.push((c.id, c.question.clone())),
        }
    }
    times.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        times
            .get(((times.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    Ok(Report {
        cases: cases.len() - skipped,
        skipped,
        hit1,
        hit5,
        mrr: if cases.len() == skipped {
            0.0
        } else {
            mrr / (cases.len() - skipped) as f64
        },
        p50_ms: pct(0.5),
        p95_ms: pct(0.95),
        misses,
    })
}
