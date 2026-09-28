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
    /// Slowest 5% of recalls across the sets (ms).
    pub p95_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Real {
    pub prompts: usize,
    pub shown: usize,
    pub helpful: usize,
    pub helped: usize,
    pub unjudged: usize,
}

/// Measure this build with its settings. Every set must exist.
pub fn metrics(conn: &Connection, judge: bool) -> Result<Metrics> {
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
    let real = if judge {
        let r = run("real-dev", Some("chain"))?;
        let j = r.judged;
        Some(Real {
            prompts: j.judged_prompts,
            shown: j.judged_shown,
            helpful: j.right,
            helped: j.helped,
            unjudged: j.unjudged,
        })
    } else {
        None
    };
    Ok(Metrics {
        version: env!("CARGO_PKG_VERSION").into(),
        model: crate::embed::model_name(),
        model_loaded: crate::recall::semantic_embedder().is_some(),
        recall_cases: recall.cases,
        recall_hit1: recall.hit1,
        recall_hit5: recall.hit5,
        vague_cases: vague.cases,
        vague_hit5: vague.hit5,
        vague_negatives: vague.negatives,
        vague_false_alarms: vague.false_alarms.len(),
        real,
        p95_ms: recall.p95_ms.max(vague.p95_ms),
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
            add(
                "real prompts: shown memories judged helpful",
                format!("{:.0}% ({}/{})", 100.0 * p(br), br.helpful, br.shown),
                format!("{:.0}% ({}/{})", 100.0 * p(cr), cr.helpful, cr.shown),
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
        "slowest 5% of recalls",
        format!("{:.0} ms", b.p95_ms),
        format!("{:.0} ms", c.p95_ms),
        "≤ 300 ms".into(),
        c.p95_ms <= 300.0,
    );
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

impl Drop for Snapshot {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

/// Metrics from a mnem binary run with `eval --gate-metrics` (and optional settings).
pub fn metrics_of(bin: &Path, db: &Path, config: Option<&Path>, judge: bool) -> Result<Metrics> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--db").arg(db).args(["eval", "--gate-metrics"]);
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
            }),
            p95_ms: 140.0,
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
        assert!(failed(&base()).is_empty());
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
            shown: 200,
            helpful: 110,
            helped: 37,
            unjudged: 1,
        });
        c.p95_ms = 301.0;
        c.model_loaded = false;
        let f = failed(&c);
        for name in [
            "known questions found in top 5",
            "no-answer prompts that recall something",
            "real prompts: shown memories judged helpful",
            "real prompts helped",
            "real prompts the judge could not judge",
            "slowest 5% of recalls",
            "embedding model",
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
    fn without_judgments_the_gate_fails() {
        let mut c = base();
        c.real = None;
        assert!(failed(&c).contains(&"real prompts (judged)"));
    }
}
