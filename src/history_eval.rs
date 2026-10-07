//! `mnem eval --history <set>`: does `mnem ask` answer real questions about past work
//! correctly, with sources, and say so when nothing is recorded?
//!
//! Each case (`~/.mnem/eval/history-dev.jsonl`, `history-test.jsonl`) is a question the
//! owner asks, the moment it is asked as of (`before`), its project, the points a correct
//! answer must contain (`expect`) and claims that make it wrong (`must_not`). It is
//! replayed through the same scope, sources and answer as `mnem ask`, so nothing written
//! after `before` is seen. A judge model then marks the answer: does it state each
//! expected point, does it make a forbidden claim, does it abstain. Judgments are cached by
//! (judge, case, answer), so a rerun with the same answer costs nothing.
//!
//! Classes: why, when, window, xproj, tried (answerable); open (nothing decided: must say
//! so without inventing one); neg (never happened: must abstain); control (not about past
//! work: no sources should be cited). The test file is looked at once per change, never
//! tuned on; tune on dev.

use crate::ask::{self, Asked, Ref};
use crate::distill::Llm;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

#[derive(Debug, Deserialize)]
pub struct Case {
    pub id: String,
    pub class: String,
    #[serde(default)]
    pub project: String,
    pub question: String,
    pub before: i64,
    #[serde(default)]
    pub expect: Vec<String>,
    #[serde(default)]
    pub must_not: Vec<String>,
    #[serde(default)]
    pub abstain: bool,
}

const JUDGE: &str = r#"You grade an answer that a memory tool gave to a developer's question about their own past work.
You get the question, the answer, the points a correct answer must contain, and claims that make it wrong.
For each expected point, mark 1 if the answer states it (same meaning, any wording; a close paraphrase counts;
a vague mention that loses the specific fact does not), else 0. Mark "violates" 1 only if the answer makes one of the
forbidden claims, or asserts a decision, reason or completion that contradicts the expected points. Extra
context consistent with the expected points (for example, that something still exists and was improved)
is not a violation.
Mark "abstains" 1 if the answer says the record does not contain an answer, or that something was not
recorded or not decided, instead of answering.
Return JSON only: {"points": [0 or 1 per expected point, in order], "violates": 0 or 1, "abstains": 0 or 1}"#;
/// Bump when JUDGE changes: cached marks of another rubric are not reused.
const JUDGE_VERSION: u32 = 2;
/// The grader: another model family than the distillation chain that writes answers.
pub const JUDGE_MODEL: &str = "developer/claude-haiku-4-5-20251001";

/// What one case got and how it was marked.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub id: String,
    pub class: String,
    pub pass: bool,
    /// Runs that passed, of `runs`.
    pub passed_runs: usize,
    pub runs: usize,
    pub points: (usize, usize),
    pub violates: bool,
    pub abstains: bool,
    pub cited: usize,
    pub ms: u128,
    pub answer: String,
}

pub fn load(path: &std::path::Path) -> Result<Vec<Case>> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&l).with_context(|| format!("bad case: {l}")))
        .collect()
}

/// Whether a marked case passes, by its class.
fn passes(class: &str, all_points: bool, violates: bool, abstains: bool, cited: usize) -> bool {
    match class {
        // Never happened: say so (abstain, or correct the premise as its points state),
        // and invent nothing.
        "neg" => (abstains || all_points) && !violates,
        // Not a question about past work: no sources.
        "control" => cited == 0,
        // Nothing decided, or an answer: every point, no forbidden claim, no refusal.
        "open" => all_points && !violates,
        _ => all_points && !violates && !abstains,
    }
}

/// How many times each case is answered. A model's answer varies run to run (on the dev
/// set one run scored anywhere from 8 to 11 of 11), so one run cannot gate a change: a
/// case passes when most of its runs pass, and the report shows how many did.
pub const RUNS: usize = 3;

/// Answer and mark every case `runs` times; one line per case on stderr as it goes.
pub fn run(
    conn: &Connection,
    llm: &Llm,
    judge: &Llm,
    cases: &[Case],
    runs: usize,
) -> Result<Vec<Outcome>> {
    let mut cache = load_cache();
    let projects = ask::projects(conn)?;
    let offset = crate::when::local_offset_min();
    let mut out = Vec::new();
    for c in cases {
        let t0 = std::time::Instant::now();
        let scope = ask::resolve(
            &c.question,
            Asked {
                project: None,
                all: false,
                here: (!c.project.is_empty()).then(|| c.project.clone()),
                projects: projects.clone(),
                since: None,
                until: None,
                as_of: Some(iso(c.before, offset)),
                now: c.before,
                offset_min: offset,
            },
        )?;
        let sources = ask::sources(conn, &c.question, &scope)?;
        // Time to find the sources, counted in every run as `mnem ask` would.
        let finding = t0.elapsed().as_millis();
        let mut tries = Vec::new();
        for _ in 0..runs.max(1) {
            let t = std::time::Instant::now();
            let (answer, cited): (String, Vec<Ref>) = if sources.is_empty() {
                ("Nothing recorded for that.".into(), vec![])
            } else {
                ask::answer(llm, &c.question, &scope, &sources)?
            };
            let ms = t.elapsed().as_millis();
            let (points, violates, abstains) = mark(judge, &mut cache, c, &answer)?;
            let got = points.iter().filter(|p| **p).count();
            let pass = passes(
                &c.class,
                got == points.len(),
                violates,
                abstains,
                cited.len(),
            );
            tries.push(Outcome {
                id: c.id.clone(),
                class: c.class.clone(),
                pass,
                passed_runs: usize::from(pass),
                runs: 1,
                points: (got, points.len()),
                violates,
                abstains,
                cited: cited.len(),
                ms: finding + ms,
                answer,
            });
        }
        let o = majority(tries);
        eprintln!(
            "{} {:<4} {:<8} {}/{} runs · points {}/{} {}{}· {} ms · {}",
            if o.pass { "PASS" } else { "FAIL" },
            o.id,
            o.class,
            o.passed_runs,
            o.runs,
            o.points.0,
            o.points.1,
            if o.violates { "VIOLATES " } else { "" },
            if o.abstains { "abstains " } else { "" },
            o.ms,
            crate::text::head(&c.question, 60)
        );
        out.push(o);
    }
    Ok(out)
}

/// One outcome for a case's runs: it passes when more than half did; its other fields
/// are from the worst run (the one a user could get), its time the median.
fn majority(mut tries: Vec<Outcome>) -> Outcome {
    let n = tries.len();
    let passed = tries.iter().filter(|o| o.pass).count();
    let mut ms: Vec<u128> = tries.iter().map(|o| o.ms).collect();
    ms.sort_unstable();
    tries.sort_by_key(|o| (o.pass, o.points.0));
    let mut o = tries.swap_remove(0);
    o.pass = passed * 2 > n;
    o.passed_runs = passed;
    o.runs = n;
    o.ms = ms[n / 2];
    o
}

/// The report: pass rate per class and overall, false answers on unanswerable cases,
/// and latency.
pub fn report(outcomes: &[Outcome]) -> String {
    let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for o in outcomes {
        let e = by.entry(o.class.as_str()).or_default();
        e.0 += usize::from(o.pass);
        e.1 += 1;
    }
    let mut s = String::new();
    for (class, (p, n)) in &by {
        s.push_str(&format!("  {class:<8} {p}/{n}\n"));
    }
    let answerable: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| !matches!(o.class.as_str(), "neg" | "control"))
        .collect();
    let unanswerable: Vec<&Outcome> = outcomes.iter().filter(|o| o.class == "neg").collect();
    let pass = |v: &[&Outcome]| v.iter().filter(|o| o.pass).count();
    let (runs_ok, runs) = outcomes
        .iter()
        .fold((0, 0), |(a, b), o| (a + o.passed_runs, b + o.runs));
    let mut ms: Vec<u128> = outcomes.iter().map(|o| o.ms).collect();
    ms.sort_unstable();
    let p = |q: f64| {
        ms.get(((ms.len().max(1) - 1) as f64 * q).round() as usize)
            .copied()
            .unwrap_or(0)
    };
    s.push_str(&format!(
        "answerable {}/{} supported and complete · unanswerable answered falsely {}/{} · all {}/{} (by majority; single runs {}/{}) · latency p50 {} ms, p95 {} ms\n",
        pass(&answerable),
        answerable.len(),
        unanswerable.iter().filter(|o| !o.pass).count(),
        unanswerable.len(),
        outcomes.iter().filter(|o| o.pass).count(),
        outcomes.len(),
        runs_ok,
        runs,
        p(0.5),
        p(0.95)
    ));
    s
}

fn iso(ms: i64, offset_min: i64) -> String {
    let local = ms + offset_min * 60_000;
    let (y, m, d) = crate::text::civil_from_days(local.div_euclid(86_400_000));
    let secs = local.rem_euclid(86_400_000) / 1000;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        if offset_min < 0 { '-' } else { '+' },
        offset_min.abs() / 60,
        offset_min.abs() % 60
    )
}

type Cache = std::collections::HashMap<String, Value>;

fn cache_path() -> std::path::PathBuf {
    crate::eval::set_path("history-judgments")
}

fn load_cache() -> Cache {
    let mut m = Cache::new();
    if let Ok(f) = std::fs::File::open(cache_path()) {
        for l in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&l)
                && let Some(k) = v["key"].as_str()
            {
                m.insert(k.to_string(), v["marks"].clone());
            }
        }
    }
    m
}

fn key(judge: &str, c: &Case, answer: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(
        format!(
            "v{JUDGE_VERSION}\n{judge}\n{}\n{}\n{}\n{answer}",
            c.question,
            c.expect.join("\n"),
            c.must_not.join("\n")
        )
        .as_bytes(),
    );
    h[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// The judge's marks for `answer`: per expected point, violation, abstention.
fn mark(judge: &Llm, cache: &mut Cache, c: &Case, answer: &str) -> Result<(Vec<bool>, bool, bool)> {
    let k = key(&judge.identity(), c, answer);
    let v = match cache.get(&k) {
        Some(v) => v.clone(),
        None => {
            let user = format!(
                "Question: {}\n\nAnswer:\n{}\n\nExpected points:\n{}\n\nForbidden claims:\n{}",
                c.question,
                answer,
                c.expect
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("{}. {p}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
                if c.must_not.is_empty() {
                    "(none)".to_string()
                } else {
                    c.must_not.join("\n")
                }
            );
            let (v, model) = judge.ask(JUDGE, &user)?;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(cache_path())?;
            writeln!(f, "{}", json!({ "key": k, "model": model, "marks": v }))?;
            cache.insert(k, v.clone());
            v
        }
    };
    let points: Vec<bool> = v["points"]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_i64() == Some(1)).collect())
        .unwrap_or_default();
    anyhow::ensure!(
        points.len() == c.expect.len(),
        "judge gave {} marks for {} points on {}",
        points.len(),
        c.expect.len(),
        c.id
    );
    Ok((
        points,
        v["violates"].as_i64() == Some(1),
        v["abstains"].as_i64() == Some(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_class_passes_on_its_own_terms() {
        // An answerable question needs every point, no false claim, no refusal.
        assert!(passes("why", true, false, false, 2));
        assert!(!passes("why", false, false, false, 2));
        assert!(!passes("why", true, true, false, 2));
        assert!(!passes("window", true, false, true, 2));
        // Nothing decided: saying so is the answer.
        assert!(passes("open", true, false, true, 1));
        assert!(!passes("open", true, true, false, 1));
        // Never happened: abstain, or correct the premise; never invent.
        assert!(passes("neg", false, false, true, 0));
        assert!(passes("neg", true, false, false, 2));
        assert!(!passes("neg", false, false, false, 3));
        assert!(!passes("neg", true, true, false, 2));
        // Not about past work: no sources cited.
        assert!(passes("control", false, false, false, 0));
        assert!(!passes("control", false, false, false, 1));
    }

    fn outcome(pass: bool, got: usize, ms: u128) -> Outcome {
        Outcome {
            id: "q".into(),
            class: "why".into(),
            pass,
            passed_runs: usize::from(pass),
            runs: 1,
            points: (got, 3),
            violates: false,
            abstains: false,
            cited: 1,
            ms,
            answer: format!("{got}"),
        }
    }

    #[test]
    fn a_case_passes_when_most_runs_do_and_shows_its_worst_run() {
        let o = majority(vec![
            outcome(true, 3, 30),
            outcome(false, 1, 10),
            outcome(true, 3, 20),
        ]);
        assert!(o.pass);
        assert_eq!((o.passed_runs, o.runs, o.points.0, o.ms), (2, 3, 1, 20));
        let o = majority(vec![
            outcome(true, 3, 1),
            outcome(false, 2, 1),
            outcome(false, 0, 1),
        ]);
        assert!(!o.pass);
        assert_eq!(o.answer, "0");
    }

    #[test]
    fn replay_times_are_written_in_the_askers_offset() {
        let ms = crate::text::parse_ts("2026-09-24T01:20:00Z").unwrap();
        assert_eq!(iso(ms, 420), "2026-09-24T08:20:00+07:00");
        assert_eq!(crate::text::parse_ts(&iso(ms, 420)), Some(ms));
        assert_eq!(iso(ms, -300), "2026-09-23T20:20:00-05:00");
    }
}
