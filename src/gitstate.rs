//! Working-tree snapshots: at the end of each agent turn, record what git says about
//! the repository (branch, unpushed commits, uncommitted files). The next session,
//! in any agent, opens knowing what was left in progress.
//!
//! A snapshot is evidence of the state at that moment, not the current state; context
//! renders it with its age.

use crate::db;
use crate::text;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Budget for all git calls of one snapshot together.
const GIT_BUDGET: Duration = Duration::from_secs(3);
const MAX_FILES: usize = 12;

/// Run git in `cwd`, giving up at `deadline`. Its output is read while it runs (a large
/// `git status` must not block on a full pipe until the deadline).
fn git(cwd: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    crate::files::run_bounded(
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0"),
        deadline.saturating_duration_since(Instant::now()),
    )
}

/// A compact, human-readable description of the working tree, or None outside git.
/// All git calls together share one GIT_BUDGET.
pub fn describe(cwd: &Path) -> Option<(String, String)> {
    let deadline = Instant::now() + GIT_BUDGET;
    let run = |args: &[&str]| git(cwd, args, deadline);
    let root = run(&["rev-parse", "--show-toplevel"])?.trim().to_string();
    let status = run(&["status", "--porcelain=v1", "-b", "--untracked-files=normal"])?;
    let mut lines = status.lines();
    // "## main...origin/main [ahead 2, behind 1]" or "## HEAD (no branch)"
    let branch = lines
        .next()
        .unwrap_or("")
        .trim_start_matches("## ")
        .to_string();
    let files: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
    let head = run(&["log", "-1", "--format=%h %s"]).unwrap_or_default();
    let unpushed = run(&["log", "--oneline", "@{upstream}..HEAD"])
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let stat = run(&["diff", "HEAD", "--shortstat"]).unwrap_or_default();

    let mut w = format!("branch {branch}\nHEAD {}", head.trim());
    if unpushed > 0 {
        w.push_str(&format!("\n{unpushed} unpushed commit(s)"));
    }
    if files.is_empty() {
        w.push_str("\nclean working tree");
    } else {
        if !stat.trim().is_empty() {
            w.push_str(&format!("\n{}", stat.trim()));
        }
        let shown: Vec<String> = files
            .iter()
            .take(MAX_FILES)
            .map(|l| l.trim().to_string())
            .collect();
        w.push_str(&format!(
            "\nuncommitted ({}): {}",
            files.len(),
            shown.join(" · ")
        ));
        if files.len() > MAX_FILES {
            w.push_str(&format!(" · +{} more", files.len() - MAX_FILES));
        }
    }
    Some((root, text::redact(&w)))
}

/// Record the working tree for a session. Returns false when nothing changed since the
/// session's last snapshot, the directory is not a git repo, or the session is unknown.
pub fn record(conn: &Connection, session: &str, cwd: &Path) -> Result<bool> {
    let known: Option<i64> = conn
        .query_row(
            "SELECT coalesce(max(turn), 0) FROM events WHERE session_id = ?1",
            params![session],
            |r| r.get(0),
        )
        .optional()?;
    let exists: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sessions WHERE id = ?1",
        params![session],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let Some((root, desc)) = describe(cwd) else {
        return Ok(false);
    };
    let last: Option<String> = conn
        .query_row(
            "SELECT text FROM events WHERE session_id = ?1 AND kind = 'git_state' ORDER BY id DESC LIMIT 1",
            params![session],
            |r| r.get(0),
        )
        .optional()?;
    if last.as_deref() == Some(desc.as_str()) {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR IGNORE INTO events(session_id, record_key, ts, turn, kind, path, text)
         VALUES (?1, ?2, ?3, ?4, 'git_state', ?5, ?6)",
        params![
            session,
            format!("git:{}", text::hash(&format!("{}{desc}", db::now_ms()))),
            db::now_ms(),
            known.unwrap_or(0),
            root,
            desc
        ],
    )?;
    Ok(true)
}

/// The newest snapshot for a project: (agent, age ms, repo root, description).
pub fn latest(conn: &Connection, project: &str) -> Result<Option<(String, i64, String, String)>> {
    Ok(conn
        .query_row(
            "SELECT s.agent, e.ts, coalesce(e.path, ''), e.text FROM events e JOIN sessions s ON s.id = e.session_id
             WHERE s.project = ?1 AND e.kind = 'git_state' AND e.ts > ?2
             ORDER BY e.ts DESC LIMIT 1",
            params![project, db::now_ms() - 7 * 86_400_000],
            |r| Ok((r.get(0)?, db::now_ms() - r.get::<_, i64>(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_a_real_repo() {
        let d = std::env::temp_dir().join(format!("mnem-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&d)
                .args(args)
                .output()
                .unwrap();
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "first",
        ]);
        std::fs::write(d.join("wip.rs"), "fn main() {}").unwrap();
        let (_, desc) = describe(&d).unwrap();
        assert!(desc.contains("branch main"), "{desc}");
        assert!(desc.contains("first"), "{desc}");
        assert!(desc.contains("uncommitted (1): ?? wip.rs"), "{desc}");
        assert!(describe(&std::env::temp_dir().join("definitely-not-a-repo-mnem")).is_none());
    }
}
