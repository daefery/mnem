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
    /// The session the prompt was typed in: its own memories are left out on replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    /// The file the agent edited after this prompt (repo-relative): the case then judges
    /// the memories about that file, as file-aware recall would offer them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
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
                            session: None,
                            file: None,
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

/// Sample `n` real human prompts from transcripts into `real-dev` (for tuning) and
/// `real-test` (look once, never tune on it), split by a hash of their session so one
/// conversation never lands in both. Each is replayed as of when it was typed, without
/// its own session's memories; its answer is unknown, so recall is judged.
pub fn build_real(conn: &Connection, n: usize) -> Result<(usize, usize)> {
    let mut st = conn.prepare(
        "SELECT e.text, e.ts, s.project, s.id FROM events e JOIN sessions s ON s.id = e.session_id
         WHERE e.kind = 'prompt' AND e.label IS NULL AND e.thread IS NULL AND e.ts IS NOT NULL
           AND length(e.text) BETWEEN 40 AND 800 AND s.project NOT LIKE '/%'
           AND s.project IN (SELECT project FROM memories GROUP BY project HAVING count(*) >= 200)
         ORDER BY abs(random()) LIMIT ?1",
    )?;
    let rows: Vec<(String, i64, String, String)> = st
        .query_map([n as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    split_real(rows, "real", |session| {
        question_key(session).ends_with(['0', '2', '4', '6', '8', 'a', 'c', 'e'])
    })
}

/// Write sampled prompts into `<name>-dev` and `<name>-test`, split by session so one
/// conversation never lands in both.
fn split_real(
    rows: Vec<(String, i64, String, String)>,
    name: &str,
    dev_half: impl Fn(&str) -> bool,
) -> Result<(usize, usize)> {
    let dir = db::data_dir().join("eval");
    std::fs::create_dir_all(&dir)?;
    let mut dev = std::fs::File::create(set_path(&format!("{name}-dev")))?;
    let mut test = std::fs::File::create(set_path(&format!("{name}-test")))?;
    let (mut a, mut b) = (0, 0);
    for (question, ts, project, session) in rows {
        let dev_half = dev_half(&session);
        let case = serde_json::to_string(&Case {
            id: None,
            ids: vec![],
            project,
            question: question.trim().to_string(),
            before: Some(ts),
            session: Some(session),
            file: None,
            open: true,
        })?;
        if dev_half {
            writeln!(dev, "{case}")?;
            a += 1;
        } else {
            writeln!(test, "{case}")?;
            b += 1;
        }
    }
    Ok((a, b))
}

/// Memories about a file offered with each edit (at most).
pub const FILE_TOP: usize = 3;

/// Prompts typed once mnem distilled sessions itself: from the last `days` days, in
/// projects that already held at least 20 memories mnem distilled when the prompt was
/// typed, so recall is measured on the memories mnem now writes (the `real` sets mostly
/// predate them and see imported claude-mem memories). No project fills more than half
/// the sample and no session more than 6 prompts; sessions are dealt to the two halves
/// largest first, each to the smaller half, since there are few and they run long.
/// Prompts an agent wrote for another agent are left out: ones that point at a Claude
/// Code scratchpad (/tmp/claude-) or ask for a verbatim reply (tool tests).
pub fn build_recent(conn: &Connection, n: usize, days: i64) -> Result<(usize, usize)> {
    let mut st = conn.prepare(
        "SELECT e.text, e.ts, s.project, s.id FROM events e JOIN sessions s ON s.id = e.session_id
         WHERE e.kind = 'prompt' AND e.label IS NULL AND e.thread IS NULL AND e.ts > ?1
           AND length(e.text) BETWEEN 40 AND 800 AND s.project NOT LIKE '/%'
           AND e.text NOT LIKE '%/tmp/claude-%' AND lower(e.text) NOT LIKE '%reply with only%'
           AND lower(e.text) NOT LIKE '%reply with exactly%'
           AND (SELECT count(*) FROM memories m WHERE m.origin = 'mnem' AND m.project = s.project
                  AND m.created_at < e.ts) >= 20
         ORDER BY abs(random())",
    )?;
    let all: Vec<(String, i64, String, String)> = st
        .query_map([db::now_ms() - days * 86_400_000], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    use std::collections::HashMap;
    let cap = n.div_ceil(2);
    let mut per_project: HashMap<String, usize> = HashMap::new();
    let mut per_session: HashMap<String, usize> = HashMap::new();
    let rows: Vec<_> = all
        .into_iter()
        .filter(|r| {
            let s = per_session.entry(r.3.clone()).or_default();
            *s += 1;
            if *s > 6 {
                return false;
            }
            let p = per_project.entry(r.2.clone()).or_default();
            *p += 1;
            *p <= cap
        })
        .take(n)
        .collect();
    let mut sizes: HashMap<&str, usize> = HashMap::new();
    for r in &rows {
        *sizes.entry(r.3.as_str()).or_default() += 1;
    }
    let mut order: Vec<(&str, usize)> = sizes.into_iter().collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let (mut dev_n, mut test_n) = (0, 0);
    let mut dev = std::collections::HashSet::new();
    for (session, k) in order {
        if dev_n <= test_n {
            dev_n += k;
            dev.insert(session.to_string());
        } else {
            test_n += k;
        }
    }
    split_real(rows, "recent", |s| dev.contains(s))
}

/// Sample `n` real file edits (the prompt of that turn and the file, repo-relative)
/// into `files-dev` and `files-test`, split by session. Each is replayed as of the edit
/// and judged against the memories about that file (`--judge`).
pub fn build_files(conn: &Connection, n: usize) -> Result<(usize, usize)> {
    let mut st = conn.prepare(
        "SELECT e.path, coalesce(s.cwd, ''), s.project, s.id, e.ts,
                (SELECT p.text FROM events p WHERE p.session_id = e.session_id AND p.kind = 'prompt'
                   AND p.label IS NULL AND p.thread IS NULL AND p.id < e.id ORDER BY p.id DESC LIMIT 1)
           FROM events e JOIN sessions s ON s.id = e.session_id
          WHERE e.kind = 'file_edit' AND e.path IS NOT NULL AND e.ts IS NOT NULL AND s.project NOT LIKE '/%'
            AND s.project IN (SELECT project FROM memories GROUP BY project HAVING count(*) >= 200)
          ORDER BY abs(random()) LIMIT ?1",
    )?;
    let rows: Vec<(String, String, String, String, i64, Option<String>)> = st
        .query_map([(n * 4) as i64], |r| {
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
    let mut dev = std::fs::File::create(set_path("files-dev"))?;
    let mut test = std::fs::File::create(set_path("files-test"))?;
    let mut seen = std::collections::HashSet::new();
    let (mut a, mut b) = (0, 0);
    for (path, cwd, project, session, ts, prompt) in rows {
        let Some(prompt) = prompt.filter(|p| p.trim().len() >= 20) else {
            continue;
        };
        // Repo-relative as far as the session's directory tells: files outside it
        // belong to another project.
        let rel = if Path::new(&path).is_absolute() {
            match Path::new(&path).strip_prefix(&cwd) {
                Ok(r) if !cwd.is_empty() => r.to_string_lossy().into_owned(),
                _ => continue,
            }
        } else {
            path.trim_start_matches("./").to_string()
        };
        if rel.is_empty() || !seen.insert((session.clone(), rel.clone())) {
            continue;
        }
        let dev_half = question_key(&session).ends_with(['0', '2', '4', '6', '8', 'a', 'c', 'e']);
        let case = serde_json::to_string(&Case {
            id: None,
            ids: vec![],
            project,
            question: crate::text::head(prompt.trim(), 1500).to_string(),
            before: Some(ts),
            session: Some(session),
            file: Some(rel),
            open: true,
        })?;
        if dev_half {
            writeln!(dev, "{case}")?;
            a += 1;
        } else {
            writeln!(test, "{case}")?;
            b += 1;
        }
        if a + b >= n {
            break;
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
/// Bump when JUDGE changes: cached judgments of another rubric are not reused.
const JUDGE_VERSION: u32 = 1;

type Judgments = std::collections::HashMap<String, bool>;

/// Cache key: the rubric, the judge, the prompt, and the exact memory text judged, so an
/// edited memory or a different judge is judged afresh.
fn judgment_key(judge: &str, question: &str, memory_text: &str) -> String {
    format!(
        "v{JUDGE_VERSION}:{judge}:{}:{}",
        question_key(question),
        crate::embed::text_hash(memory_text)
    )
}

/// Relevance of each memory to the prompt, judged by `judge_name`'s models and cached in
/// ~/.mnem/eval/judgments.jsonl so reruns cost nothing. None if the judge failed or
/// answered anything but one 0 or 1 per memory.
#[allow(clippy::too_many_arguments)]
fn judge(
    conn: &Connection,
    llm: &Llm,
    judge_name: &str,
    cache: &mut Judgments,
    project: &str,
    question: &str,
    ids: &[i64],
) -> Result<Option<Vec<bool>>> {
    let mut texts = Vec::new();
    for id in ids {
        let (t, s, n): (String, String, String) = conn.query_row(
            "SELECT coalesce(title, ''), coalesce(subtitle, ''), coalesce(narrative, '') FROM memories WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        texts.push(format!("{t} — {s} — {}", crate::text::head(&n, 300)));
    }
    let keys: Vec<String> = texts
        .iter()
        .map(|t| judgment_key(judge_name, question, t))
        .collect();
    let todo: Vec<usize> = (0..ids.len())
        .filter(|i| !cache.contains_key(&keys[*i]))
        .collect();
    if !todo.is_empty() {
        let lines: Vec<String> = todo
            .iter()
            .enumerate()
            .map(|(n, i)| format!("{}. {}", n + 1, texts[*i]))
            .collect();
        let user = format!(
            "Project: {project}\nPrompt:\n{}\n\nMemories:\n{}",
            crate::text::head(question, 1500),
            lines.join("\n")
        );
        let Ok((v, model)) = llm.ask(JUDGE, &user) else {
            return Ok(None);
        };
        let Some(marks) = parse_marks(&v, todo.len()) else {
            return Ok(None);
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(set_path("judgments"))?;
        for (i, rel) in todo.iter().zip(marks) {
            writeln!(
                f,
                "{}",
                json!({ "key": keys[*i], "model": model, "relevant": rel })
            )?;
            cache.insert(keys[*i].clone(), rel);
        }
    }
    Ok(Some(keys.iter().map(|k| cache[k]).collect()))
}

/// Exactly `n` marks, each the number 0 or 1; anything else is no judgment at all.
fn parse_marks(v: &Value, n: usize) -> Option<Vec<bool>> {
    let marks: Vec<bool> = v["relevant"]
        .as_array()?
        .iter()
        .map(|x| match x.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (marks.len() == n).then_some(marks)
}

fn load_judgments() -> Judgments {
    let mut m = Judgments::new();
    if let Ok(f) = std::fs::File::open(set_path("judgments")) {
        for l in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&l)
                && let (Some(k), Some(rel)) = (v["key"].as_str(), v["relevant"].as_bool())
            {
                m.insert(k.to_string(), rel);
            }
        }
    }
    m
}

/// The identity judgments are cached under for `--judge` ("chain") or one model.
pub fn judge_identity(name: &str) -> Result<String> {
    let l = Llm::from_config()?;
    Ok(if name == "chain" { l } else { l.only(name) }.identity())
}

/// How often two judges (by identity) agree on the memories both judged:
/// (shared, agreed, Cohen's kappa).
pub fn agreement(a: &str, b: &str) -> (usize, usize, f64) {
    let all = load_judgments();
    let (pa, pb) = (
        format!("v{JUDGE_VERSION}:{a}:"),
        format!("v{JUDGE_VERSION}:{b}:"),
    );
    let pairs: Vec<(bool, bool)> = all
        .iter()
        .filter_map(|(k, x)| {
            let rest = k.strip_prefix(&pa)?;
            all.get(&format!("{pb}{rest}")).map(|y| (*x, *y))
        })
        .collect();
    let n = pairs.len();
    if n == 0 {
        return (0, 0, 0.0);
    }
    let agreed = pairs.iter().filter(|(x, y)| x == y).count();
    let (ya, yb) = (
        pairs.iter().filter(|p| p.0).count() as f64 / n as f64,
        pairs.iter().filter(|p| p.1).count() as f64 / n as f64,
    );
    let po = agreed as f64 / n as f64;
    let pe = ya * yb + (1.0 - ya) * (1.0 - yb);
    (
        n,
        agreed,
        if pe >= 1.0 {
            1.0
        } else {
            (po - pe) / (1.0 - pe)
        },
    )
}

/// Cosine of each memory's text, embedded now with `e`, to the query.
fn candidate_texts(conn: &Connection, ids: &[i64]) -> Result<Vec<String>> {
    let mut texts = Vec::new();
    for id in ids {
        texts.push(conn.query_row(
            "SELECT coalesce(title, ''), coalesce(subtitle, ''), coalesce(narrative, ''), coalesce(facts, '[]') FROM memories WHERE id = ?1",
            [id],
            |r| {
                Ok(crate::embed::memory_text(
                    &r.get::<_, String>(0)?,
                    &r.get::<_, String>(1)?,
                    &r.get::<_, String>(2)?,
                    &r.get::<_, String>(3)?,
                ))
            },
        )?);
    }
    Ok(texts)
}

fn candidate_cosines(
    conn: &Connection,
    e: &crate::embed::Embedder,
    q: &crate::embed::Query,
    ids: &[i64],
) -> Result<Vec<Option<f32>>> {
    let texts = candidate_texts(conn, ids)?;
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    let qn = norm(&q.vec);
    Ok(e.embed(&texts)
        .iter()
        .map(|v| {
            (v.len() == q.vec.len())
                .then(|| v.iter().zip(&q.vec).map(|(a, b)| a * b).sum::<f32>() / (norm(v) * qn))
        })
        .collect())
}

/// One dumped prompt: its candidates' (cosine, judged helpful).
type Dumped = (String, Vec<(Option<f32>, Option<bool>)>);

/// `field` is the score to read: "cos" keeps the keyword order (the gate filters it),
/// any other score (e.g. "rerank") reorders the candidates by that score, as recall
/// does with a reranker.
fn read_dump(path: &Path, field: &str) -> Result<Vec<Dumped>> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for l in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&l)?;
        let mut cands: Vec<(Option<f32>, Option<bool>)> = v["cands"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| (c[field].as_f64().map(|x| x as f32), c["relevant"].as_bool()))
                    .collect()
            })
            .unwrap_or_default();
        if field != "cos" {
            cands.sort_by(|a, b| b.0.unwrap_or(-1.0).total_cmp(&a.0.unwrap_or(-1.0)));
        }
        out.push((v["q"].as_str().unwrap_or_default().to_string(), cands));
    }
    Ok(out)
}

/// Probability that a helpful candidate scores above an unhelpful one (ties half).
fn auc<'a>(cases: impl Iterator<Item = &'a Dumped>) -> Option<f64> {
    let (mut pos, mut neg) = (Vec::new(), Vec::new());
    for (_, cands) in cases {
        for (cos, rel) in cands {
            if let (Some(c), Some(r)) = (cos, rel) {
                if *r { pos.push(*c) } else { neg.push(*c) }
            }
        }
    }
    if pos.is_empty() || neg.is_empty() {
        return None;
    }
    let wins: f64 = pos
        .iter()
        .map(|p| {
            neg.iter()
                .map(|n| {
                    if p > n {
                        1.0
                    } else if p == n {
                        0.5
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
        })
        .sum();
    Some(wins / (pos.len() * neg.len()) as f64)
}

/// Compare `mnem eval --dump` files from different models: AUC of cosine for the judged
/// candidates, a prompt-level bootstrap interval for each model's AUC minus the first
/// one's, and what a relevance threshold would keep of each prompt's top five.
pub fn analyze(paths: &[std::path::PathBuf], field: &str) -> Result<String> {
    // A path may name its score as path:field, to compare a reranker with a gate.
    let dumps: Vec<(String, Vec<Dumped>)> = paths
        .iter()
        .map(|p| {
            let s = p.to_string_lossy();
            let (path, f) = match s.rsplit_once(':') {
                Some((a, b)) if !b.contains('/') => (std::path::PathBuf::from(a), b.to_string()),
                _ => (p.clone(), field.to_string()),
            };
            let name = format!(
                "{}[{f}]",
                path.file_stem().unwrap_or_default().to_string_lossy()
            );
            Ok((name, read_dump(&path, &f)?))
        })
        .collect::<Result<_>>()?;
    let mut out = String::new();
    let Some((base_name, base)) = dumps.first() else {
        return Ok(out);
    };
    let base_by_q: std::collections::HashMap<&str, &Dumped> =
        base.iter().map(|d| (d.0.as_str(), d)).collect();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for (name, cases) in &dumps {
        let a = auc(cases.iter()).unwrap_or(f64::NAN);
        out.push_str(&format!("{name}: AUC {a:.3} over {} prompts", cases.len()));
        if name != base_name {
            // Resample prompts (not candidates: those of one prompt move together).
            let shared: Vec<(&Dumped, &Dumped)> = cases
                .iter()
                .filter_map(|d| base_by_q.get(d.0.as_str()).map(|b| (d, *b)))
                .collect();
            let mut diffs = Vec::new();
            for _ in 0..1000 {
                let pick: Vec<usize> = (0..shared.len())
                    .map(|_| (next() % shared.len().max(1) as u64) as usize)
                    .collect();
                if let (Some(x), Some(y)) = (
                    auc(pick.iter().map(|i| shared[*i].0)),
                    auc(pick.iter().map(|i| shared[*i].1)),
                ) {
                    diffs.push(x - y);
                }
            }
            diffs.sort_by(f64::total_cmp);
            if !diffs.is_empty() {
                let q = |p: f64| diffs[((diffs.len() - 1) as f64 * p) as usize];
                out.push_str(&format!(
                    " · minus {base_name}: {:+.3} (95% CI {:+.3} to {:+.3}, above zero in {:.0}% of resamples)",
                    diffs.iter().sum::<f64>() / diffs.len() as f64,
                    q(0.025),
                    q(0.975),
                    100.0 * diffs.iter().filter(|d| **d > 0.0).count() as f64 / diffs.len() as f64
                ));
            }
        }
        out.push('\n');
        for step in 0..=19 {
            let t = step as f32 * 0.05;
            let (mut right, mut wrong, mut helped) = (0, 0, 0);
            for (_, cands) in cases {
                let kept: Vec<&(Option<f32>, Option<bool>)> = cands
                    .iter()
                    .filter(|(c, _)| c.is_none_or(|c| c >= t))
                    .take(5)
                    .collect();
                let r = kept.iter().filter(|(_, j)| *j == Some(true)).count();
                right += r;
                wrong += kept.iter().filter(|(_, j)| *j == Some(false)).count();
                helped += (r > 0) as usize;
            }
            out.push_str(&format!(
                "  threshold {t:.2}: {right} helpful, {wrong} unhelpful shown, {helped} prompts helped\n"
            ));
        }
    }
    Ok(out)
}

/// 95% Wilson interval for k successes out of n.
pub fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let (z, n, p) = (1.96f64, n as f64, k as f64 / n as f64);
    let d = 1.0 + z * z / n;
    let c = p + z * z / (2.0 * n);
    let r = z * ((p * (1.0 - p) + z * z / (4.0 * n)) / n).sqrt();
    ((c - r) / d, (c + r) / d)
}

/// Recall on real prompts as judged: memories shown, how many helped, and how many
/// prompts got at least one useful memory.
#[derive(Default)]
pub struct Judged {
    pub prompts: usize,
    pub shown: usize,
    /// Prompts whose memories were judged, and the memories in them.
    pub judged_prompts: usize,
    pub judged_shown: usize,
    pub right: usize,
    pub helped: usize,
    /// Prompts whose judgment failed: counted neither way.
    pub unjudged: usize,
    /// File cases: memories offered that prompt recall had already offered.
    pub overlap: usize,
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
    /// Time to rerank each dumped prompt's candidates, sorted (ms).
    pub rerank_ms: Vec<f64>,
}

/// `judge`: None, or the judge to use: "chain" (the configured distillation models) or
/// one model name.
/// `dump`: also write, per real prompt, its top ten candidates with their cosine to
/// the prompt under the configured model (embedded now, not read from the index) and
/// their judgment, so thresholds and models can be compared offline.
pub fn run(
    conn: &Connection,
    path: &Path,
    mode: recall::Mode,
    judge_with: Option<&str>,
    dump: Option<&Path>,
    rerank: Option<&crate::rerank::Reranker>,
) -> Result<Report> {
    let mut rerank_ms: Vec<f64> = Vec::new();
    let mut dump = dump.map(std::fs::File::create).transpose()?;
    let llm = match judge_with {
        Some(j) => {
            // The gate pins the judge to the live settings (MNEM_JUDGE_CONFIG) so a
            // candidate's settings cannot change who grades it.
            let l = match std::env::var_os("MNEM_JUDGE_CONFIG") {
                Some(p) => {
                    let cfg: crate::config::Config =
                        serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| {
                            format!("read judge settings {}", p.to_string_lossy())
                        })?)?;
                    Llm::from_distill(&cfg.distill)?
                }
                None => Llm::from_config()?,
            };
            l.load_cooldowns(conn);
            Some(if j == "chain" { l } else { l.only(j) })
        }
        None => None,
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
        // A replayed prompt sees what existed when it was typed, minus its own session.
        let scope = recall::Scope {
            session: c.session.as_deref(),
            offered_to: None,
            before: c.before,
        };
        let ranked = recall::rank(
            conn,
            &c.project,
            &c.question,
            &scope,
            10,
            query.as_ref(),
            mode,
        )?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let targets: Vec<i64> = c.id.into_iter().chain(c.ids.iter().copied()).collect();
        if c.open {
            if let Some(out) = dump.as_mut() {
                let cands: Vec<i64> = ranked.iter().take(10).map(|r| r.0).collect();
                let marks = match &llm {
                    Some(llm) if !cands.is_empty() => judge(
                        conn,
                        llm,
                        &llm.identity(),
                        &mut cache,
                        &c.project,
                        &c.question,
                        &cands,
                    )?,
                    _ => None,
                };
                let cos = match (&embedder, &query) {
                    (Some(e), Some(q)) => candidate_cosines(conn, e, q, &cands)?,
                    _ => vec![None; cands.len()],
                };
                let rerank: Vec<Option<f32>> = match rerank {
                    Some(r) => {
                        let t = Instant::now();
                        let s = r.scores(&c.question, &candidate_texts(conn, &cands)?)?;
                        rerank_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                        s.into_iter().map(Some).collect()
                    }
                    None => vec![None; cands.len()],
                };
                let rows: Vec<Value> = cands
                    .iter()
                    .enumerate()
                    .map(|(i, id)| {
                        json!({ "id": id, "rank": i, "cos": cos[i], "rerank": rerank[i],
                                "relevant": marks.as_ref().map(|m| m[i]) })
                    })
                    .collect();
                writeln!(
                    out,
                    "{}",
                    json!({ "q": question_key(&c.question), "project": c.project, "cands": rows })
                )?;
            }
            // A file case judges the memories about the file, with the prompt and the
            // file as context; a prompt case judges what prompt recall showed.
            // The repository on this machine, from the session's directory, so paths are
            // placed as they are in production.
            let case_root: Option<std::path::PathBuf> = c.file.as_ref().and_then(|_| {
                let cwd: String = conn
                    .query_row(
                        "SELECT cwd FROM sessions WHERE id = ?1",
                        [c.session.as_deref().unwrap_or("")],
                        |r| r.get(0),
                    )
                    .ok()?;
                crate::files::repo_root(Path::new(&cwd))
            });
            let (top, question): (Vec<i64>, String) = match &c.file {
                Some(f) => {
                    let ids: Vec<i64> = crate::files::about_in(
                        conn,
                        &crate::files::Place {
                            rel: f,
                            root: case_root.as_deref(),
                            project: &c.project,
                        },
                        &scope,
                        FILE_TOP,
                    )?
                    .iter()
                    .map(|a| a.id)
                    .collect();
                    judged.overlap += ids
                        .iter()
                        .filter(|id| ranked.iter().take(5).any(|r| r.0 == **id))
                        .count();
                    (
                        ids,
                        // MNEM_EVAL_NO_FILE_HINT: judge without naming the file, a control
                        // for how much the hint alone makes memories look relevant.
                        if std::env::var_os("MNEM_EVAL_NO_FILE_HINT").is_some() {
                            c.question.clone()
                        } else {
                            format!(
                                "{}\n\n(The agent is now working on the file {f}.)",
                                c.question
                            )
                        },
                    )
                }
                None => (
                    ranked.iter().take(5).map(|r| r.0).collect(),
                    c.question.clone(),
                ),
            };
            judged.prompts += 1;
            judged.shown += top.len();
            if let Some(llm) = &llm
                && !top.is_empty()
            {
                match judge(
                    conn,
                    llm,
                    &llm.identity(),
                    &mut cache,
                    &c.project,
                    &question,
                    &top,
                )? {
                    Some(marks) => {
                        let right = marks.iter().filter(|m| **m).count();
                        judged.judged_prompts += 1;
                        judged.judged_shown += top.len();
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
        rerank_ms: {
            rerank_ms.sort_by(f64::total_cmp);
            rerank_ms
        },
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
        let hits = crate::embed::search(conn, &q, &c.project, &recall::Scope::default(), 500)?;
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
    use super::{Case, auc, parse_marks, wilson};
    use serde_json::json;

    #[test]
    fn a_rerank_score_reorders_candidates_a_cosine_does_not() {
        let f = std::env::temp_dir().join(format!("mnem-dump-{}.jsonl", std::process::id()));
        std::fs::write(
            &f,
            r#"{"q":"a","cands":[{"cos":0.9,"rerank":0.1,"relevant":false},{"cos":0.2,"rerank":0.8,"relevant":true}]}"#,
        )
        .unwrap();
        let by_cos = super::read_dump(&f, "cos").unwrap();
        assert_eq!(by_cos[0].1[0].1, Some(false), "keyword order kept");
        let by_rerank = super::read_dump(&f, "rerank").unwrap();
        assert_eq!(
            by_rerank[0].1[0],
            (Some(0.8), Some(true)),
            "best score first"
        );
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn judge_answers_are_strict() {
        assert_eq!(
            parse_marks(&json!({"relevant": [1, 0]}), 2),
            Some(vec![true, false])
        );
        for bad in [
            json!({"relevant": ["1", 0]}),
            json!({"relevant": [1, 2]}),
            json!({"relevant": [1]}),
            json!({"relevant": [true, false]}),
            json!({}),
        ] {
            assert_eq!(parse_marks(&bad, 2), None, "{bad}");
        }
        let d = |c: Vec<(Option<f32>, Option<bool>)>| (String::new(), c);
        let perfect = [d(vec![(Some(0.9), Some(true)), (Some(0.1), Some(false))])];
        assert_eq!(auc(perfect.iter()), Some(1.0));
        let tie = [d(vec![
            (Some(0.5), Some(true)),
            (Some(0.5), Some(false)),
            (None, Some(true)),
        ])];
        assert_eq!(auc(tie.iter()), Some(0.5));
        let (lo, hi) = wilson(50, 100);
        assert!((lo - 0.404).abs() < 0.01 && (hi - 0.596).abs() < 0.01);
    }

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
        assert!(real.open && real.before == Some(5) && real.session.is_none());
        // Written back, defaults stay out of the file.
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"id":7,"project":"p","question":"why"}"#
        );
    }
}

const ASK_FROM_DIGEST: &str = r#"You write evaluation questions for a memory search system used by software developers.
Given the digest of one past AI coding session, write the question a developer would type to
their coding agent weeks later that this session's durable outcome answers (a decision, a fix,
how something works). Describe the problem or goal as a real person would, not the answer.
One sentence, 8-25 words. If nothing durable happened, return {"question": null}.
Return JSON only: {"question": "..."}"#;

const CHOOSE: &str = r#"A coding agent is about to answer a developer. It was shown past memories by id,
type and title only, and can open any of them in full. Which would it open because they likely
help answer the question? Return JSON only: {"open": [ids]} (an empty list if none)."#;

/// Writes the questions and picks what to open; not the distillation model.
const TITLES_JUDGE: &str = "developer/claude-haiku-4-5-20251001";
const TITLES_DISTILLER: &str = "gpt-5.6-luna";

#[derive(Default, Debug)]
pub struct TitleArm {
    pub hit1: usize,
    pub hit5: usize,
    pub mrr: f64,
    /// A target was among the five memories shown.
    pub shown: usize,
    /// The judge, seeing titles only, chose to open a target.
    pub opened: usize,
    /// Memories the judge chose to open that were not targets.
    pub opened_other: usize,
}

/// Compare the distillation title rule with the candidate on the same session chunks.
/// On a frozen copy of the database, `n` chunks the live prompt distilled are distilled
/// again with the candidate rule (same model) into a second copy; a different model
/// writes the question each chunk answers from its digest, never from either memory.
/// Both copies are then scored with prompt recall as the hook runs it (top five), and on
/// whether that model, shown only the titles, would open a memory from the chunk.
pub fn titles(live: &Path, n: usize, out: &Path) -> Result<(usize, TitleArm, TitleArm)> {
    let snap_a = crate::gate::Snapshot::take(live)?;
    let snap_b = snap_a.copy()?;
    let a = db::open(&snap_a.0)?;
    let mut b = db::open(&snap_b.0)?;
    let distiller = Llm::from_config()?.only(TITLES_DISTILLER);
    let judge = Llm::from_config()?.only(TITLES_JUDGE);
    let system = crate::distill::SYSTEM
        .replace(crate::distill::TITLE_RULE, crate::distill::TITLE_RULE_NAMED);
    anyhow::ensure!(
        system != crate::distill::SYSTEM,
        "title rule not found in the prompt"
    );

    // Chunks the current prompt distilled (memories with evidence links), by that model,
    // at most a quarter from one project.
    let groups: Vec<(String, String)> = {
        let mut st = a.prepare(
            "SELECT DISTINCT substr(m.origin_id, 1, instr(m.origin_id, '#') - 1), m.project
             FROM memories m
             WHERE m.origin = 'mnem' AND m.kind = 'observation' AND m.model = ?1
               AND m.project NOT LIKE '/%' AND instr(m.origin_id, '#') > 0
               AND EXISTS (SELECT 1 FROM memory_evidence e WHERE e.memory_id = m.id)
             ORDER BY abs(random())",
        )?;
        st.query_map([TITLES_DISTILLER], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let per_project = n.div_ceil(4).max(1);
    let mut taken: std::collections::HashMap<String, usize> = Default::default();
    let targets = |conn: &Connection, base: &str| -> Result<Vec<(i64, String)>> {
        let mut st = conn.prepare(
            "SELECT id, title FROM memories WHERE origin = 'mnem' AND kind = 'observation'
               AND origin_id LIKE ?1 || '#%' ORDER BY id",
        )?;
        Ok(st
            .query_map([base], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let mut cases: Vec<(String, String, Vec<i64>, Vec<i64>)> = Vec::new();
    let mut log = std::fs::File::create(out)?;
    for (base, project) in groups {
        if cases.len() >= n {
            break;
        }
        if taken.get(&project).copied().unwrap_or(0) >= per_project {
            continue;
        }
        let Some((sid, range)) = base.rsplit_once('@') else {
            continue;
        };
        let Some((from, through)) = range
            .split_once('-')
            .and_then(|(f, t)| Some((f.parse::<i64>().ok()?, t.parse::<i64>().ok()?)))
        else {
            continue;
        };
        // The chunk must come out of the digest builder exactly as it was distilled.
        let Some(c) = crate::distill::chunks(&a, sid, from - 1)?
            .into_iter()
            .next()
            .filter(|c| c.from == from && c.through == through)
        else {
            continue;
        };
        let (agent, title): (String, String) = a.query_row(
            "SELECT agent, coalesce(title, '') FROM sessions WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let user = crate::distill::digest_prompt(&project, &agent, &title, &c.text);
        let Ok((q, _)) = judge.ask(ASK_FROM_DIGEST, &user) else {
            continue;
        };
        let Some(question) = q["question"].as_str().filter(|s| s.len() > 10) else {
            continue;
        };
        let (v, model) = match distiller.ask(&system, &user) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{base}: {e:#}");
                continue;
            }
        };
        b.execute(
            "DELETE FROM memories WHERE origin = 'mnem' AND origin_id LIKE ?1 || '#%'",
            [&base],
        )?;
        crate::distill::store(&mut b, sid, &project, &model, &c, &v)?;
        let old = targets(&a, &base)?;
        let new = targets(&b, &base)?;
        if old.is_empty() || new.is_empty() {
            continue;
        }
        writeln!(
            log,
            "{}",
            json!({ "chunk": base, "project": project, "question": question,
                    "old": old.iter().map(|t| &t.1).collect::<Vec<_>>(),
                    "new": new.iter().map(|t| &t.1).collect::<Vec<_>>() })
        )?;
        *taken.entry(project.clone()).or_default() += 1;
        cases.push((
            project,
            question.to_string(),
            old.into_iter().map(|t| t.0).collect(),
            new.into_iter().map(|t| t.0).collect(),
        ));
        eprint!("\rtitles: {} of {n} chunks", cases.len());
    }
    eprintln!();
    // The new memories need vectors, as the watcher would give them.
    if let Some(e) = recall::semantic_embedder() {
        crate::embed::backfill(&mut b, &e, None)?;
    }
    let embedder = recall::semantic_embedder();
    let (mut old, mut new) = (TitleArm::default(), TitleArm::default());
    for (project, question, want_a, want_b) in &cases {
        for (arm, conn, want) in [(&mut old, &a, want_a), (&mut new, &b, want_b)] {
            let s = score_titles(conn, &judge, embedder.as_ref(), project, question, want)?;
            arm.hit1 += s.hit1;
            arm.hit5 += s.hit5;
            arm.mrr += s.mrr;
            arm.shown += s.shown;
            arm.opened += s.opened;
            arm.opened_other += s.opened_other;
        }
    }
    Ok((cases.len(), old, new))
}

/// Prompt recall as the hook runs it (top five) for one question, and whether the judge,
/// shown only the titles, would open one of the memories that answer it.
fn score_titles(
    conn: &Connection,
    judge: &Llm,
    embedder: Option<&crate::embed::Embedder>,
    project: &str,
    question: &str,
    want: &[i64],
) -> Result<TitleArm> {
    let query = embedder.map(|e| e.query(question));
    let scope = recall::Scope {
        session: None,
        offered_to: None,
        before: None,
    };
    let ranked = recall::rank(
        conn,
        project,
        question,
        &scope,
        10,
        query.as_ref(),
        recall::Mode::Fill,
    )?;
    let mut arm = TitleArm::default();
    if let Some(pos) = ranked.iter().position(|r| want.contains(&r.0)) {
        arm.hit1 += (pos == 0) as usize;
        arm.hit5 += (pos < 5) as usize;
        arm.mrr += 1.0 / (pos + 1) as f64;
    }
    let top: Vec<i64> = ranked.iter().take(5).map(|r| r.0).collect();
    arm.shown += top.iter().any(|id| want.contains(id)) as usize;
    if top.is_empty() {
        return Ok(arm);
    }
    let mut lines = format!("Question: {question}\n\nMemories:\n");
    for id in &top {
        let (ty, title): (String, String) = conn.query_row(
            "SELECT coalesce(type, kind), title FROM memories WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        lines.push_str(&format!("#{id} {ty} · {title}\n"));
    }
    if let Ok((v, _)) = judge.ask(CHOOSE, &lines) {
        let open: Vec<i64> = v["open"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| {
                x.as_i64()
                    .or_else(|| x.as_str()?.trim_start_matches('#').parse().ok())
            })
            .collect();
        arm.opened += open.iter().any(|id| want.contains(id)) as usize;
        arm.opened_other += open.iter().filter(|id| !want.contains(id)).count();
    }
    Ok(arm)
}

const RETITLE: &str = r#"You rewrite the titles of stored engineering memories. Keep what each memory says;
change only its title, following this rule:
{rule}
Return JSON only: {"titles": [{"id": 123, "title": "..."}]}, one entry per memory given."#;

/// Titles alone: the chunks and questions of an earlier `--titles` run (its side-by-side
/// file), every memory kept as it is and only its title rewritten under the candidate
/// rule in a second copy, so content, count and everything else stay the same.
pub fn retitle(live: &Path, compare: &Path, out: &Path) -> Result<(usize, TitleArm, TitleArm)> {
    let snap_a = crate::gate::Snapshot::take(live)?;
    let snap_b = snap_a.copy()?;
    let a = db::open(&snap_a.0)?;
    let mut b = db::open(&snap_b.0)?;
    let writer = Llm::from_config()?.only(TITLES_DISTILLER);
    let judge = Llm::from_config()?.only(TITLES_JUDGE);
    let system = RETITLE.replace("{rule}", crate::distill::TITLE_RULE_NAMED);
    let prior: Vec<Value> = std::io::BufReader::new(std::fs::File::open(compare)?)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    let mut log = std::fs::File::create(out)?;
    let mut cases: Vec<(String, String, Vec<i64>)> = Vec::new();
    for p in &prior {
        let (Some(base), Some(project), Some(question)) = (
            p["chunk"].as_str(),
            p["project"].as_str(),
            p["question"].as_str(),
        ) else {
            continue;
        };
        type Mem = (i64, String, String, String, String);
        let mems: Vec<Mem> = {
            let mut st = a.prepare(
                "SELECT id, title, coalesce(subtitle, ''), coalesce(narrative, ''), coalesce(facts, '[]')
                 FROM memories WHERE origin = 'mnem' AND kind = 'observation'
                   AND origin_id LIKE ?1 || '#%' ORDER BY id",
            )?;
            st.query_map([base], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        if mems.is_empty() {
            continue;
        }
        let input = json!({ "project": project, "memories": mems.iter().map(|m| json!({
            "id": m.0, "title": m.1, "subtitle": m.2, "narrative": m.3, "facts": m.4 }))
            .collect::<Vec<_>>() });
        let v = match writer.ask(&system, &input.to_string()) {
            Ok((v, _)) => v,
            Err(e) => {
                eprintln!("{base}: {e:#}");
                continue;
            }
        };
        let mut new = Vec::new();
        for t in v["titles"].as_array().into_iter().flatten() {
            let (Some(id), Some(title)) = (t["id"].as_i64(), t["title"].as_str()) else {
                continue;
            };
            if mems.iter().any(|m| m.0 == id) && !title.trim().is_empty() {
                b.execute(
                    "UPDATE memories SET title = ?2 WHERE id = ?1",
                    params![id, title.trim()],
                )?;
                new.push(title.trim().to_string());
            }
        }
        writeln!(
            log,
            "{}",
            json!({ "chunk": base, "project": project, "question": question,
                    "old": mems.iter().map(|m| &m.1).collect::<Vec<_>>(), "new": new })
        )?;
        cases.push((
            project.into(),
            question.into(),
            mems.iter().map(|m| m.0).collect(),
        ));
        eprint!("\rretitle: {} of {} chunks", cases.len(), prior.len());
    }
    eprintln!();
    // Rewritten titles lose their vectors (trigger); embed them again.
    if let Some(e) = recall::semantic_embedder() {
        crate::embed::backfill(&mut b, &e, None)?;
    }
    let embedder = recall::semantic_embedder();
    let (mut old, mut new) = (TitleArm::default(), TitleArm::default());
    for (project, question, want) in &cases {
        for (arm, conn) in [(&mut old, &a), (&mut new, &b)] {
            let s = score_titles(conn, &judge, embedder.as_ref(), project, question, want)?;
            arm.hit1 += s.hit1;
            arm.hit5 += s.hit5;
            arm.mrr += s.mrr;
            arm.shown += s.shown;
            arm.opened += s.opened;
            arm.opened_other += s.opened_other;
        }
    }
    Ok((cases.len(), old, new))
}
