//! The recall release gate: before a change to recall ships, run the installed mnem
//! (the baseline) and the candidate build (or candidate settings) on the same database
//! and the same cached judgments, and refuse the change if recall got worse.
//!
//! Comparing two builds side by side, rather than against numbers stored earlier,
//! keeps the gate honest while the database grows: new memories move every metric a
//! little, and they move both runs alike. The real-prompt test half is never used here,
//! so gating many times cannot tune recall to it.

use crate::eval::{self, set_path};
use crate::recall::Mode;
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What the gate measures, from one build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub version: String,
    pub model: String,
    /// The embedding model actually loaded (a build without ONNX cannot load MiniLM).
    #[serde(default)]
    pub model_loaded: bool,
    /// Model-written known-item questions.
    pub recall_cases: usize,
    pub recall_hit1: usize,
    pub recall_hit5: usize,
    /// Hand-written vague questions, and prompts no memory answers.
    pub vague_cases: usize,
    pub vague_hit5: usize,
    pub vague_negatives: usize,
    pub vague_false_alarms: usize,
    /// Real prompts (tuning half), judged; None when not judged.
    pub real: Option<Real>,
    /// Slowest 5% of rankings on the known and vague questions (ms, in process).
    pub p95_ms: f64,
    /// MCP search on the known questions.
    #[serde(default)]
    pub search: Option<Search>,
    /// Prompt recall as a hook runs it: process start plus recall through the build's
    /// own background service.
    #[serde(default)]
    pub hook: Option<Hook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Real {
    pub prompts: usize,
    pub shown: usize,
    pub helpful: usize,
    pub helped: usize,
    pub unjudged: usize,
    /// Prompts that were shown memories, none of them helpful (None from older builds).
    #[serde(default)]
    pub unhelpful_only: Option<usize>,
    /// Every real prompt in the set, judged or not (None from older builds).
    #[serde(default)]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Search {
    pub cases: usize,
    pub hit5: usize,
    pub hit20: usize,
    /// Personal-detail memories that search listed without being asked for them.
    pub sensitive_leaks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hook {
    pub prompts: usize,
    pub p95_ms: f64,
    /// Prompts for which the service gave no query vector (recall fell back to keywords).
    pub fallbacks: usize,
}

fn cases(set: &str) -> Result<Vec<serde_json::Value>> {
    Ok(std::fs::read_to_string(set_path(set))?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// Memory ids listed by an MCP search answer (#123 lines).
fn listed(answer: &str) -> Vec<i64> {
    answer
        .lines()
        .filter_map(|l| l.strip_prefix('#')?.split(' ').next()?.parse().ok())
        .collect()
}

/// Measure this build with its settings on the database `db`. Every set must exist
/// and be large enough to mean something.
pub fn metrics(conn: &Connection, db: &Path, judge: bool) -> Result<Metrics> {
    for (set, how) in [
        ("recall", "mnem eval --build 40"),
        ("vague", "write it by hand (see README)"),
        ("real-dev", "mnem eval --build-real 160"),
    ] {
        if !set_path(set).exists() {
            bail!("the gate needs the {set} test set: {how}");
        }
    }
    let run =
        |set: &str, j: Option<&str>| eval::run(conn, &set_path(set), Mode::Fill, j, None, None);
    let recall = run("recall", None)?;
    let vague = run("vague", None)?;
    if recall.cases + recall.skipped < 20 || vague.cases < 10 || vague.negatives < 5 {
        bail!(
            "the test sets are too small to gate on (recall {}, vague {} + {} no-answer; need 20, 10 and 5)",
            recall.cases + recall.skipped,
            vague.cases,
            vague.negatives
        );
    }
    let p95 = recall.p95_ms.max(vague.p95_ms);
    let real = if judge {
        // Its ranking time is not gated here: replaying old prompts measures word rarity
        // as of their date, which production never pays. The hook check below times real
        // prompts through the production path instead.
        let r = run("real-dev", Some("chain"))?;
        let j = r.judged;
        if j.prompts < 30 {
            bail!(
                "the real-dev set has {} prompts; the gate needs 30",
                j.prompts
            );
        }
        Some(Real {
            prompts: j.judged_prompts,
            shown: j.judged_shown,
            helpful: j.right,
            helped: j.helped,
            unjudged: j.unjudged,
            unhelpful_only: Some(j.judged_prompts - j.helped),
            total: Some(j.prompts),
        })
    } else {
        None
    };
    let model_loaded = crate::recall::semantic_embedder().is_some();

    // This build's own background service on a free port, so search and hook recall use
    // this build's model exactly as they would once installed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let served = db.to_path_buf();
    std::thread::spawn(move || crate::ui::serve(served, port, || {}));
    // SAFETY: set before any other thread of this process reads the environment.
    unsafe { std::env::set_var("MNEM_UI_PORT", port.to_string()) };
    if model_loaded {
        let t = std::time::Instant::now();
        while crate::embed::query_from_service(conn, "warm up").is_none() {
            if t.elapsed() > std::time::Duration::from_secs(180) {
                bail!("this build's service did not serve its embedding model within 3 minutes");
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    // MCP search on the known questions, and whether it lists personal details.
    let sensitive = |id: i64| -> bool {
        conn.query_row(
            "SELECT coalesce(type, '') = 'sensitive' FROM memories WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap_or(false)
    };
    let (mut hit5, mut hit20, mut n, mut leaks) = (0, 0, 0, 0);
    for set in ["recall", "vague"] {
        for c in cases(set)? {
            let (Some(q), Some(project)) = (c["question"].as_str(), c["project"].as_str()) else {
                continue;
            };
            let answer = crate::mcp::call(
                conn,
                "search",
                &serde_json::json!({ "query": q, "project": project, "type": "observations", "limit": 20 }),
            )?;
            let ids = listed(&answer);
            leaks += ids.iter().filter(|id| sensitive(**id)).count();
            if set == "recall"
                && let Some(target) = c["id"].as_i64().filter(|t| !sensitive(*t))
            {
                n += 1;
                if let Some(i) = ids.iter().position(|id| *id == target) {
                    hit5 += (i < 5) as usize;
                    hit20 += 1;
                }
            }
        }
    }

    // Prompt recall the way the hook runs it: a fresh process of this build per real
    // prompt (start-up, settings, database open, recall through this build's service),
    // each a fresh session so nothing is held back as already offered.
    let exe = std::env::current_exe()?;
    let (mut times, mut fallbacks) = (Vec::new(), 0);
    for (i, c) in cases("real-dev")?.iter().enumerate() {
        let (Some(q), Some(project)) = (c["question"].as_str(), c["project"].as_str()) else {
            continue;
        };
        let t = std::time::Instant::now();
        let out = std::process::Command::new(&exe)
            .arg("--db")
            .arg(db)
            .args([
                "recall-probe",
                "--session",
                &format!("gate:{i}"),
                "--project",
                project,
                "--prompt",
                q,
            ])
            .output()?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if !out.status.success() {
            bail!(
                "recall failed for a real prompt: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if model_loaded
            && q.split_whitespace().count() >= 4
            && crate::embed::query_from_service(conn, q).is_none()
        {
            fallbacks += 1;
        }
    }
    times.sort_by(f64::total_cmp);
    let hook = Hook {
        prompts: times.len(),
        p95_ms: times
            .get(((times.len().max(1) - 1) as f64 * 0.95) as usize)
            .copied()
            .unwrap_or(0.0),
        fallbacks,
    };

    Ok(Metrics {
        version: env!("CARGO_PKG_VERSION").into(),
        model: crate::embed::model_name(),
        model_loaded,
        recall_cases: recall.cases,
        recall_hit1: recall.hit1,
        recall_hit5: recall.hit5,
        vague_cases: vague.cases,
        vague_hit5: vague.hit5,
        vague_negatives: vague.negatives,
        vague_false_alarms: vague.false_alarms.len(),
        real,
        p95_ms: p95,
        search: Some(Search {
            cases: n,
            hit5,
            hit20,
            sensitive_leaks: leaks,
        }),
        hook: Some(hook),
    })
}

/// One rule's outcome.
pub struct Check {
    pub name: &'static str,
    pub baseline: String,
    pub candidate: String,
    pub limit: String,
    pub pass: bool,
}

/// Limits sized to the sets' noise: one case is 2.6% of the 38 recall questions and
/// 5% of the 20 vague ones, so a single case of movement is tolerated; anything more
/// is a regression. Prompts that recall something with no answer may not exceed 3 of
/// 10 (where MiniLM stands) or grow by more than one. Judged precision may drop 3
/// points; the latency ceiling is the hook's 300 ms budget.
pub fn compare(b: &Metrics, c: &Metrics) -> Vec<Check> {
    let mut out = Vec::new();
    let mut add = |name, baseline: String, candidate: String, limit: String, pass| {
        out.push(Check {
            name,
            baseline,
            candidate,
            limit,
            pass,
        })
    };
    let at_least = |n: usize, k: usize| n.saturating_sub(k);
    // Both builds must have been graded on the same cases.
    // A number an older baseline did not measure is left out, not taken as zero.
    let totals = (
        b.real.as_ref().and_then(|r| r.total),
        c.real.as_ref().and_then(|r| r.total),
    );
    let sizes = |m: &Metrics, real: Option<usize>| {
        format!(
            "{}+{}{}",
            m.recall_cases,
            m.vague_cases + m.vague_negatives,
            match (totals.0.is_some() && totals.1.is_some(), real) {
                (true, Some(t)) => format!("+{t}"),
                _ => String::new(),
            }
        )
    };
    add(
        "same test cases for both builds",
        sizes(b, totals.0),
        sizes(c, totals.1),
        "equal".into(),
        sizes(b, totals.0) == sizes(c, totals.1),
    );
    let loaded = |m: &Metrics| {
        if m.model_loaded {
            "loads"
        } else {
            "does not load"
        }
        .to_string()
    };
    add(
        "embedding model",
        format!("{} {}", b.model, loaded(b)),
        format!("{} {}", c.model, loaded(c)),
        "loads if the baseline's does".into(),
        c.model_loaded || !b.model_loaded,
    );
    add(
        "known questions found in top 5",
        format!("{}/{}", b.recall_hit5, b.recall_cases),
        format!("{}/{}", c.recall_hit5, c.recall_cases),
        format!("≥ {}", at_least(b.recall_hit5, 1)),
        c.recall_hit5 >= at_least(b.recall_hit5, 1),
    );
    add(
        "known questions found first",
        format!("{}/{}", b.recall_hit1, b.recall_cases),
        format!("{}/{}", c.recall_hit1, c.recall_cases),
        format!("≥ {}", at_least(b.recall_hit1, 2)),
        c.recall_hit1 >= at_least(b.recall_hit1, 2),
    );
    add(
        "vague questions found in top 5",
        format!("{}/{}", b.vague_hit5, b.vague_cases),
        format!("{}/{}", c.vague_hit5, c.vague_cases),
        format!("≥ {}", at_least(b.vague_hit5, 1)),
        c.vague_hit5 >= at_least(b.vague_hit5, 1),
    );
    // At most 3 and at most one more than the baseline; above 3 it may only improve.
    let fa_limit = if b.vague_false_alarms <= 3 {
        3.min(b.vague_false_alarms + 1)
    } else {
        b.vague_false_alarms
    };
    add(
        "no-answer prompts that recall something",
        format!("{}/{}", b.vague_false_alarms, b.vague_negatives),
        format!("{}/{}", c.vague_false_alarms, c.vague_negatives),
        format!("≤ {fa_limit}"),
        c.vague_false_alarms <= fa_limit,
    );
    match (&b.real, &c.real) {
        (Some(br), Some(cr)) => {
            let p = |r: &Real| r.helpful as f64 / r.shown.max(1) as f64;
            // The 95% interval is shown beside the share: a 3-point limit is tighter than
            // the noise at this size, so a narrow failure deserves a human look.
            let share = |r: &Real| {
                let (lo, hi) = crate::eval::wilson(r.helpful, r.shown);
                format!("{:.0}% [{:.0}-{:.0}]", 100.0 * p(r), 100.0 * lo, 100.0 * hi)
            };
            let unhelpful = |r: &Real| r.shown - r.helpful;
            let cap = unhelpful(br) + unhelpful(br) / 10 + 3;
            add(
                "real prompts: unhelpful memories shown",
                unhelpful(br).to_string(),
                unhelpful(cr).to_string(),
                format!("≤ {cap}"),
                unhelpful(cr) <= cap,
            );
            if let (Some(bu), Some(cu)) = (br.unhelpful_only, cr.unhelpful_only) {
                add(
                    "real prompts shown only unhelpful memories",
                    bu.to_string(),
                    cu.to_string(),
                    format!("≤ {}", bu + 2),
                    cu <= bu + 2,
                );
            }
            add(
                "real prompts: shown memories judged helpful",
                share(br),
                share(cr),
                format!("≥ {:.0}%", 100.0 * (p(br) - 0.03)),
                p(cr) >= p(br) - 0.03,
            );
            add(
                "real prompts helped",
                format!("{}/{}", br.helped, br.prompts),
                format!("{}/{}", cr.helped, cr.prompts),
                format!("≥ {}", at_least(br.helped, 2)),
                cr.helped >= at_least(br.helped, 2),
            );
            add(
                "real prompts the judge could not judge",
                br.unjudged.to_string(),
                cr.unjudged.to_string(),
                "0".into(),
                cr.unjudged == 0,
            );
        }
        _ => add(
            "real prompts (judged)",
            "-".into(),
            "-".into(),
            "judged".into(),
            false,
        ),
    }
    add(
        "slowest 5% of rankings",
        format!("{:.0} ms", b.p95_ms),
        format!("{:.0} ms", c.p95_ms),
        "≤ 300 ms".into(),
        c.p95_ms <= 300.0,
    );
    let dash = || "-".to_string();
    match &c.search {
        Some(cs) => {
            let bs = b.search.as_ref();
            let (b5, b20) = bs.map(|x| (x.hit5, x.hit20)).unwrap_or((0, 0));
            add(
                "MCP search: known questions in top 5",
                bs.map(|x| format!("{}/{}", x.hit5, x.cases))
                    .unwrap_or_else(dash),
                format!("{}/{}", cs.hit5, cs.cases),
                format!("≥ {}", at_least(b5, 1)),
                cs.hit5 >= at_least(b5, 1),
            );
            add(
                "MCP search: known questions in top 20",
                bs.map(|x| format!("{}/{}", x.hit20, x.cases))
                    .unwrap_or_else(dash),
                format!("{}/{}", cs.hit20, cs.cases),
                format!("≥ {}", at_least(b20, 1)),
                cs.hit20 >= at_least(b20, 1),
            );
            add(
                "MCP search: personal details listed unasked",
                bs.map(|x| x.sensitive_leaks.to_string())
                    .unwrap_or_else(dash),
                cs.sensitive_leaks.to_string(),
                "0".into(),
                cs.sensitive_leaks == 0,
            );
        }
        None => add("MCP search", dash(), dash(), "measured".into(), false),
    }
    match &c.hook {
        Some(ch) => {
            let bh = b.hook.as_ref();
            add(
                "hook recall, slowest 5% (start-up + service)",
                bh.map(|x| format!("{:.0} ms", x.p95_ms))
                    .unwrap_or_else(dash),
                format!("{:.0} ms", ch.p95_ms),
                "≤ 300 ms".into(),
                ch.p95_ms <= 300.0,
            );
            let allowed = bh.map(|x| x.fallbacks).unwrap_or(0).max(1);
            add(
                "hook recall fell back to keywords",
                bh.map(|x| x.fallbacks.to_string()).unwrap_or_else(dash),
                ch.fallbacks.to_string(),
                format!("≤ {allowed}"),
                ch.fallbacks <= allowed,
            );
        }
        None => add("hook recall", dash(), dash(), "measured".into(), false),
    }
    out
}

/// A frozen copy of the live database for one gate run, removed when dropped: both
/// builds are measured on exactly the same data even while sessions keep writing.
pub struct Snapshot(pub std::path::PathBuf);

impl Snapshot {
    pub fn take(live: &Path) -> Result<Snapshot> {
        let path = crate::db::data_dir().join(format!(".gate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Connection::open_with_flags(live, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?
            .execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
        Ok(Snapshot(path))
    }
}

impl Snapshot {
    /// A second, independent copy.
    pub fn copy(&self) -> Result<Snapshot> {
        let path = self.0.with_extension("copy.db");
        std::fs::copy(&self.0, &path)?;
        Ok(Snapshot(path))
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

/// Metrics from a mnem binary run with `eval --gate-metrics` (and optional settings).
pub fn metrics_of(
    bin: &Path,
    db: &Path,
    config: Option<&Path>,
    judge_config: &Path,
    judge: bool,
) -> Result<Metrics> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--db")
        .arg(db)
        .args(["eval", "--gate-metrics"])
        .env("MNEM_JUDGE_CONFIG", judge_config)
        .env_remove("MNEM_UI_PORT");
    if !judge {
        cmd.arg("--no-judge");
    }
    match config {
        Some(c) => cmd.env("MNEM_CONFIG", c),
        None => cmd.env_remove("MNEM_CONFIG"),
    };
    let out = cmd
        .output()
        .with_context(|| format!("run {}", bin.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("unexpected argument") {
            bail!(
                "{} predates the recall gate; install a build that has it first, then gate changes against it",
                bin.display()
            );
        }
        bail!("{} failed: {}", bin.display(), err.trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with('{'))
        .context("no metrics in the output")?;
    Ok(serde_json::from_str(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Metrics {
        Metrics {
            version: "0".into(),
            model: "m".into(),
            model_loaded: true,
            recall_cases: 38,
            recall_hit1: 21,
            recall_hit5: 33,
            vague_cases: 20,
            vague_hit5: 12,
            vague_negatives: 10,
            vague_false_alarms: 3,
            real: Some(Real {
                prompts: 44,
                shown: 182,
                helpful: 120,
                helped: 40,
                unjudged: 0,
                unhelpful_only: Some(4),
                total: Some(70),
            }),
            p95_ms: 140.0,
            search: Some(Search {
                cases: 38,
                hit5: 30,
                hit20: 34,
                sensitive_leaks: 0,
            }),
            hook: Some(Hook {
                prompts: 70,
                p95_ms: 60.0,
                fallbacks: 0,
            }),
        }
    }

    fn failed(c: &Metrics) -> Vec<&'static str> {
        compare(&base(), c)
            .into_iter()
            .filter(|k| !k.pass)
            .map(|k| k.name)
            .collect()
    }

    #[test]
    fn same_numbers_pass_and_one_case_of_noise_is_tolerated() {
        assert!(failed(&base()).is_empty(), "{:?}", failed(&base()));
        let mut c = base();
        c.recall_hit5 -= 1;
        c.vague_hit5 -= 1;
        c.recall_hit1 -= 2;
        assert!(failed(&c).is_empty(), "{:?}", failed(&c));
    }

    #[test]
    fn regressions_fail() {
        let mut c = base();
        c.recall_hit5 -= 2;
        c.vague_false_alarms = 4;
        c.real = Some(Real {
            prompts: 44,
            shown: 230,
            helpful: 110,
            helped: 37,
            unjudged: 1,
            unhelpful_only: Some(7),
            total: Some(70),
        });
        c.p95_ms = 301.0;
        c.model_loaded = false;
        c.search = Some(Search {
            cases: 38,
            hit5: 28,
            hit20: 32,
            sensitive_leaks: 1,
        });
        c.hook = Some(Hook {
            prompts: 70,
            p95_ms: 320.0,
            fallbacks: 3,
        });
        let f = failed(&c);
        for name in [
            "known questions found in top 5",
            "no-answer prompts that recall something",
            "real prompts: shown memories judged helpful",
            "real prompts helped",
            "real prompts the judge could not judge",
            "real prompts: unhelpful memories shown",
            "real prompts shown only unhelpful memories",
            "slowest 5% of rankings",
            "embedding model",
            "MCP search: known questions in top 5",
            "MCP search: known questions in top 20",
            "MCP search: personal details listed unasked",
            "hook recall, slowest 5% (start-up + service)",
            "hook recall fell back to keywords",
        ] {
            assert!(f.contains(&name), "{name} should fail: {f:?}");
        }
    }

    #[test]
    fn false_alarms_may_grow_by_one_up_to_three_and_never_above_a_high_baseline() {
        let mut b = base();
        b.vague_false_alarms = 1;
        let mut c = b.clone();
        c.vague_false_alarms = 2;
        assert!(compare(&b, &c).iter().all(|k| k.pass));
        c.vague_false_alarms = 3;
        assert!(compare(&b, &c).iter().any(|k| !k.pass));
        b.vague_false_alarms = 5;
        c.vague_false_alarms = 5;
        assert!(compare(&b, &c).iter().all(|k| k.pass));
        c.vague_false_alarms = 6;
        assert!(compare(&b, &c).iter().any(|k| !k.pass));
    }

    #[test]
    fn numbers_an_older_baseline_lacks_are_skipped() {
        let mut b = base();
        if let Some(r) = b.real.as_mut() {
            r.unhelpful_only = None;
            r.total = None;
        }
        let names: Vec<&str> = compare(&b, &base()).iter().map(|k| k.name).collect();
        assert!(!names.contains(&"real prompts shown only unhelpful memories"));
        assert!(compare(&b, &base()).iter().all(|k| k.pass));
    }

    #[test]
    fn different_test_sets_fail() {
        let mut c = base();
        c.recall_cases = 37;
        assert!(failed(&c).contains(&"same test cases for both builds"));
    }

    #[test]
    fn without_judgments_or_measurements_the_gate_fails() {
        let mut c = base();
        c.real = None;
        c.search = None;
        c.hook = None;
        let f = failed(&c);
        for name in ["real prompts (judged)", "MCP search", "hook recall"] {
            assert!(f.contains(&name), "{name}: {f:?}");
        }
    }

    #[test]
    fn listed_reads_search_ids() {
        assert_eq!(
            listed("note\n#12 [bugfix] x\nE5 [prompt] y\n#7 [x] z"),
            vec![12, 7]
        );
    }
}
