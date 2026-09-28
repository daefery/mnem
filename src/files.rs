//! File-aware recall: the memories about one file, and whether that file changed after
//! each memory was written.
//!
//! Memories list the files they read or modified (`memory_files`, kept by triggers).
//! Paths come in every shape (repo-relative, absolute on this or another machine,
//! relative to a package directory), so a memory's path matches the file when one path
//! ends with the other at a directory boundary: `src/embed.rs` matches
//! `/Users/x/code/mnem/src/embed.rs`. A lone file name (`README.md`) must match the
//! file's repo-relative path exactly. Only memories of the same project count.
//!
//! Staleness comes from git: the commits that touched the file after the memory's
//! session ended, and uncommitted edits. A memory about a file that changed since may
//! describe code that is no longer there.

use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

/// Path components, without `.`/`~` prefixes or empty parts.
fn parts(p: &str) -> Vec<&str> {
    p.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != "." && *c != "~")
        .collect()
}

/// Does a path recorded in a memory name the file at `rel` (repo-relative)?
pub fn matches(recorded: &str, rel: &str) -> bool {
    let (a, b) = (parts(recorded), parts(rel));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    // A bare file name only counts when it is the whole repo-relative path.
    if short.len() == 1 {
        return a == b;
    }
    long.ends_with(short)
}

/// A memory about a file.
#[derive(Debug, Clone)]
pub struct About {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub created_at: i64,
    /// When what the memory describes happened: its session's end when known.
    pub as_of: i64,
    pub modified: bool,
}

/// The project id without a checkout suffix (`remote#dir` → `remote`).
fn base(project: &str) -> &str {
    project.split('#').next().unwrap_or(project)
}

/// Memories of `project` (any checkout of it) that read or modified the file at `rel`,
/// those that modified it first, newest first; personal-detail memories are left out.
pub fn about(conn: &Connection, project: &str, rel: &str, limit: usize) -> Result<Vec<About>> {
    about_in(conn, project, rel, &crate::recall::Scope::default(), limit)
}

/// `about` within a recall scope: the asking session's own memories, memories already
/// offered to it, and memories created after `scope.before` are left out.
pub fn about_in(
    conn: &Connection,
    project: &str,
    rel: &str,
    scope: &crate::recall::Scope,
    limit: usize,
) -> Result<Vec<About>> {
    let name = parts(rel).last().copied().unwrap_or_default().to_string();
    let mut st = conn.prepare_cached(
        "SELECT f.memory_id, f.modified, f.path, coalesce(m.type, m.kind), coalesce(m.title, ''),
                coalesce(m.created_at, 0), coalesce(min(s.last_event_at, m.created_at), m.created_at, 0)
           FROM memory_files f
           JOIN memories m ON m.id = f.memory_id
           LEFT JOIN sessions s ON s.id = m.session_id
          WHERE f.name = ?1 AND (m.project = ?2 OR m.project LIKE ?2 || '#%')
            AND m.kind != 'pinned' AND coalesce(m.type, '') != 'sensitive'
            AND (?3 = '' OR coalesce(m.session_id, '') != ?3) AND coalesce(m.created_at, 0) < ?4
            AND NOT EXISTS (SELECT 1 FROM recall_seen r WHERE r.session_id = ?5 AND r.memory_id = m.id)
          ORDER BY m.created_at DESC",
    )?;
    let mut out: Vec<About> = Vec::new();
    let rows = st.query_map(
        params![
            name,
            base(project),
            scope.session.unwrap_or(""),
            scope.before.unwrap_or(i64::MAX),
            scope.offered_to.unwrap_or("")
        ],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        },
    )?;
    for row in rows {
        let (id, modified, path, kind, title, created_at, as_of) = row?;
        if !matches(&path, rel) {
            continue;
        }
        match out.iter_mut().find(|a| a.id == id) {
            Some(a) => a.modified |= modified,
            None => out.push(About {
                id,
                kind,
                title,
                created_at,
                as_of,
                modified,
            }),
        }
    }
    // Memories that changed the file first; newest first within each.
    out.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then(b.created_at.cmp(&a.created_at))
    });
    out.truncate(limit);
    Ok(out)
}

/// What happened to a file in git: commit times (ms) newest first, uncommitted edits,
/// and whether it still exists.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub commits: Vec<i64>,
    pub uncommitted: bool,
    pub exists: bool,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repository containing `path` (a file or directory).
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    // The file may be gone: walk up to a directory that exists.
    let dir = dir.ancestors().find(|d| d.is_dir())?.to_path_buf();
    git(&dir, &["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(s.trim()))
}

/// The file's history in `root`, `rel` being repo-relative.
pub fn history(root: &Path, rel: &str) -> History {
    let commits = git(root, &["log", "--format=%ct", "-n", "500", "--", rel])
        .map(|s| {
            s.lines()
                .filter_map(|l| l.trim().parse::<i64>().ok().map(|t| t * 1000))
                .collect()
        })
        .unwrap_or_default();
    let uncommitted =
        git(root, &["status", "--porcelain", "--", rel]).is_some_and(|s| !s.trim().is_empty());
    History {
        commits,
        uncommitted,
        exists: root.join(rel).exists(),
    }
}

/// Lines added and removed in the file since `as_of` (committed and not), from the
/// last commit before then to the working tree.
pub fn lines_since(root: &Path, rel: &str, as_of: i64) -> Option<(u64, u64)> {
    let base = git(
        root,
        &[
            "rev-list",
            "-1",
            &format!("--before={}", as_of / 1000),
            "HEAD",
        ],
    )?;
    let base = base.trim();
    if base.is_empty() {
        return None;
    }
    let stat = git(root, &["diff", "--numstat", base, "--", rel])?;
    let (mut added, mut removed) = (0, 0);
    for l in stat.lines() {
        let mut f = l.split_whitespace();
        added += f.next()?.parse::<u64>().unwrap_or(0);
        removed += f.next()?.parse::<u64>().unwrap_or(0);
    }
    Some((added, removed))
}

/// Whether anything happened to the file after `as_of`.
fn changed(h: &History, as_of: i64) -> bool {
    h.commits.iter().any(|t| *t > as_of) || h.uncommitted || !h.exists
}

/// How the file changed after `as_of`, in words; None when it did not. `lines` is the
/// size of the change when known.
pub fn change_since(
    h: &History,
    as_of: i64,
    now: i64,
    lines: Option<(u64, u64)>,
) -> Option<String> {
    let after: Vec<i64> = h.commits.iter().copied().filter(|t| *t > as_of).collect();
    let mut parts = Vec::new();
    if !h.exists {
        parts.push("the file no longer exists".to_string());
    }
    if let Some(latest) = after.first() {
        parts.push(format!(
            "{} commit{} (latest {} ago)",
            after.len(),
            if after.len() == 1 { "" } else { "s" },
            crate::context::ago(now - latest)
        ));
    }
    if h.uncommitted {
        parts.push("uncommitted edits".into());
    }
    if parts.is_empty() {
        return None;
    }
    match lines {
        Some((0, 0)) if h.exists => parts.push("no net change in content".into()),
        Some((a, r)) => parts.push(format!("+{a} −{r} lines")),
        None => {}
    }
    Some(parts.join(", "))
}

/// A file as the caller named it, resolved on this machine: repository root,
/// repo-relative path, and project id.
pub struct Target {
    pub root: PathBuf,
    pub rel: String,
    pub project: String,
}

pub fn resolve(path: &str, cwd: &Path) -> Option<Target> {
    let full = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let root = repo_root(&full)?;
    let rel = canonical(&full)
        .strip_prefix(canonical(&root))
        .ok()?
        .to_string_lossy()
        .into_owned();
    if rel.is_empty() {
        return None;
    }
    let project =
        crate::project::Resolver::default().resolve(Some(&root.to_string_lossy()), None)?;
    Some(Target { root, rel, project })
}

/// `p` with its deepest existing ancestor canonicalized (the file itself may be gone).
fn canonical(p: &Path) -> PathBuf {
    for anc in p.ancestors() {
        if let Ok(c) = anc.canonicalize() {
            return match p.strip_prefix(anc) {
                Ok(rest) if !rest.as_os_str().is_empty() => c.join(rest),
                _ => c,
            };
        }
    }
    p.to_path_buf()
}

/// What an agent is told when it first touches a file in a session: up to `limit`
/// memories about it (not its own session's, none offered before), each with how the
/// file changed since. The memories are marked offered. None when there are none.
pub fn on_touch(
    conn: &Connection,
    session: &str,
    t: &Target,
    limit: usize,
) -> Result<Option<String>> {
    let first = conn.execute(
        "INSERT OR IGNORE INTO file_seen(session_id, path) VALUES (?1, ?2)",
        params![session, format!("{}:{}", t.project, t.rel)],
    )? > 0;
    if !first {
        return Ok(None);
    }
    let found = about_in(
        conn,
        &t.project,
        &t.rel,
        &crate::recall::Scope::session(session),
        limit,
    )?;
    if found.is_empty() {
        return Ok(None);
    }
    let h = history(&t.root, &t.rel);
    let now = crate::db::now_ms();
    let mut w = format!(
        "mnem: past memories about {} (full text: get_observations([ids]))\n",
        t.rel
    );
    for a in &found {
        w.push_str(&format!(
            "#{} {} · {} · {}\n",
            a.id,
            a.kind,
            crate::context::ago(now - a.created_at),
            crate::text::head(&a.title, 110)
        ));
        let lines = changed(&h, a.as_of)
            .then(|| lines_since(&t.root, &t.rel, a.as_of))
            .flatten();
        if let Some(c) = change_since(&h, a.as_of, now, lines) {
            w.push_str(&format!("   file changed since: {c}\n"));
        }
        conn.execute(
            "INSERT OR IGNORE INTO recall_seen(session_id, memory_id) VALUES (?1, ?2)",
            params![session, a.id],
        )?;
    }
    Ok(Some(w))
}

/// Memories about a file with how it changed since each, as text for an agent.
pub fn report(conn: &Connection, t: &Target, limit: usize) -> Result<String> {
    let found = about(conn, &t.project, &t.rel, limit)?;
    if found.is_empty() {
        return Ok(format!("No memories about {} in {}.", t.rel, t.project));
    }
    let h = history(&t.root, &t.rel);
    let now = crate::db::now_ms();
    let mut w = format!(
        "Memories about {} ({}), those that changed it first:\n",
        t.rel, t.project
    );
    let mut stale = 0;
    for a in &found {
        w.push_str(&format!(
            "#{} [{}] {} · {} · {}\n",
            a.id,
            a.kind,
            crate::context::ago(now - a.created_at),
            if a.modified { "modified it" } else { "read it" },
            crate::text::head(&a.title, 140)
        ));
        let lines = changed(&h, a.as_of)
            .then(|| lines_since(&t.root, &t.rel, a.as_of))
            .flatten();
        match change_since(&h, a.as_of, now, lines) {
            Some(c) => {
                stale += 1;
                w.push_str(&format!("   changed since: {c}\n"));
            }
            None => w.push_str("   file unchanged since\n"),
        }
    }
    if stale > 0 {
        w.push_str(&format!(
            "\n{stale} of {} may describe code that has since changed.\n",
            found.len()
        ));
    }
    w.push_str("Next: get_observations([ids]) for full text.");
    Ok(w)
}

/// For `get_observations`: how the files a memory touched changed since, for those
/// that can be found on this machine (at most `max` files).
pub fn staleness_lines(conn: &Connection, memory_id: i64, max: usize) -> Result<Vec<String>> {
    let (project, as_of): (String, i64) = conn.query_row(
        "SELECT coalesce(m.project, ''), coalesce(min(s.last_event_at, m.created_at), m.created_at, 0)
           FROM memories m LEFT JOIN sessions s ON s.id = m.session_id WHERE m.id = ?1",
        [memory_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Where this project lives on this machine: the newest session's working directory.
    let cwd: Option<String> = conn
        .query_row(
            "SELECT cwd FROM sessions WHERE (project = ?1 OR project LIKE ?2 || '#%') AND cwd IS NOT NULL
             ORDER BY last_event_at DESC LIMIT 1",
            params![project, base(&project)],
            |r| r.get(0),
        )
        .ok();
    let Some(root) = cwd.and_then(|c| repo_root(Path::new(&c))) else {
        return Ok(vec![]);
    };
    let mut st = conn.prepare_cached(
        "SELECT path FROM memory_files WHERE memory_id = ?1 ORDER BY modified DESC LIMIT ?2",
    )?;
    let paths: Vec<String> = st
        .query_map(params![memory_id, (max * 2) as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let now = crate::db::now_ms();
    let mut out = Vec::new();
    for p in paths {
        let Some(rel) = local_rel(&root, &p) else {
            continue;
        };
        let h = history(&root, &rel);
        let lines = changed(&h, as_of)
            .then(|| lines_since(&root, &rel, as_of))
            .flatten();
        if let Some(c) = change_since(&h, as_of, now, lines) {
            out.push(format!("{rel}: {c}"));
        }
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

/// A recorded path as a repo-relative path under `root`, if that file exists here (or
/// existed in git).
fn local_rel(root: &Path, recorded: &str) -> Option<String> {
    let comps = parts(recorded);
    // The longest tail of the recorded path that names a file here.
    (0..comps.len()).find_map(|i| {
        let rel = comps[i..].join("/");
        (comps.len() - i >= 2 || i == 0)
            .then_some(())
            .filter(|_| root.join(&rel).exists())
            .map(|_| rel)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_at_directory_boundaries() {
        assert!(matches("src/embed.rs", "src/embed.rs"));
        assert!(matches("/Users/x/code/mnem/src/embed.rs", "src/embed.rs"));
        assert!(matches("./src/embed.rs", "src/embed.rs"));
        // Recorded relative to a package directory.
        assert!(matches(
            "src/feedback.ts",
            "packages/contracts/src/feedback.ts"
        ));
        assert!(!matches("src/embed.rs", "src/embedder.rs"));
        assert!(!matches("xsrc/embed.rs", "src/embed.rs"));
        // A bare name only matches the same bare repo path.
        assert!(matches("README.md", "README.md"));
        assert!(!matches("README.md", "docs/README.md"));
        assert!(!matches("/a/b/README.md", "README.md"));
    }

    #[test]
    fn change_is_counted_after_the_memory_only() {
        let h = History {
            commits: vec![3_000, 2_000, 1_000],
            uncommitted: false,
            exists: true,
        };
        assert_eq!(change_since(&h, 3_000, 10_000, None), None);
        assert_eq!(
            change_since(&h, 1_500, 10_000, Some((12, 3))).unwrap(),
            "2 commits (latest 7s ago), +12 −3 lines"
        );
        let gone = History {
            exists: false,
            ..Default::default()
        };
        assert_eq!(
            change_since(&gone, 0, 0, None).unwrap(),
            "the file no longer exists"
        );
    }

    #[test]
    fn about_finds_memories_of_the_project_only() {
        let c =
            crate::db::open_with(Path::new(":memory:"), std::time::Duration::from_secs(1)).unwrap();
        let add = |id: i64, project: &str, files: &str, created: i64| {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, files_modified, origin, origin_id, created_at)
                 VALUES (?1, ?2, 'observation', 'bugfix', 't', ?3, 'mnem', ?1, ?4)",
                params![id, project, files, created],
            )
            .unwrap();
        };
        add(1, "github.com/a/r", r#"["/home/x/r/src/embed.rs"]"#, 10);
        add(2, "github.com/a/r#wt", r#"["src/embed.rs"]"#, 20);
        add(3, "github.com/b/other", r#"["src/embed.rs"]"#, 30);
        add(4, "github.com/a/r", r#"["src/embedder.rs"]"#, 40);
        c.execute(
            "INSERT INTO memories(id, project, kind, type, title, files_read, origin, origin_id, created_at)
             VALUES (5, 'github.com/a/r', 'observation', 'discovery', 'r', '[\"src/embed.rs\"]', 'mnem', 5, 50)",
            [],
        )
        .unwrap();
        let found: Vec<(i64, bool)> = about(&c, "github.com/a/r", "src/embed.rs", 10)
            .unwrap()
            .iter()
            .map(|a| (a.id, a.modified))
            .collect();
        // Modifiers first, newest first; the other project and the other file are out.
        assert_eq!(found, vec![(2, true), (1, true), (5, false)]);
        // The index follows edits and deletes.
        c.execute("UPDATE memories SET files_modified = '[]' WHERE id = 2", [])
            .unwrap();
        c.execute("DELETE FROM memories WHERE id = 1", []).unwrap();
        let ids: Vec<i64> = about(&c, "github.com/a/r", "src/embed.rs", 10)
            .unwrap()
            .iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec![5]);
    }

    #[test]
    fn report_marks_memories_older_than_the_files_changes() {
        let dir = std::env::temp_dir().join(format!("mnem-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let sh_at = |args: &[&str], date: Option<&str>| {
            let mut cmd = std::process::Command::new("git");
            if let Some(d) = date {
                cmd.env("GIT_AUTHOR_DATE", d).env("GIT_COMMITTER_DATE", d);
            }
            assert!(
                cmd.arg("-C")
                    .arg(&dir)
                    .args([
                        "-c",
                        "user.name=t",
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "commit.gpgsign=false"
                    ])
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        let sh = |args: &[&str]| sh_at(args, None);
        sh(&["init", "-q"]);
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        sh(&["add", "."]);
        // The first commit is ten minutes old: the memory below comes after it.
        let old = format!("@{} +0000", crate::db::now_ms() / 1000 - 600);
        sh_at(&["commit", "-qm", "one"], Some(&old));
        let t = resolve("src/a.rs", &dir).expect("in a repository");
        assert_eq!(t.rel, "src/a.rs");
        let c =
            crate::db::open_with(Path::new(":memory:"), std::time::Duration::from_secs(1)).unwrap();
        let add = |id: i64, created: i64| {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, files_modified, origin, origin_id, created_at)
                 VALUES (?1, ?2, 'observation', 'bugfix', ?3, '[\"src/a.rs\"]', 'mnem', ?1, ?4)",
                params![id, t.project, format!("memory {id}"), created],
            )
            .unwrap();
        };
        // Written two minutes before the next commit (git time has second precision).
        add(1, crate::db::now_ms() - 120_000);
        std::fs::write(dir.join("src/a.rs"), "fn a() { b() }\nfn b() {}\n").unwrap();
        sh(&["commit", "-qam", "two"]);
        // Back to what the memory saw: commits, but no net change.
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        let r = report(&c, &t, 10).unwrap();
        assert!(r.contains("#1 [bugfix]"), "{r}");
        assert!(r.contains("changed since: 1 commit"), "{r}");
        assert!(r.contains("uncommitted edits"), "{r}");
        assert!(r.contains("no net change in content"), "{r}");
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\nfn c() {}\n").unwrap();
        let r = report(&c, &t, 10).unwrap();
        assert!(r.contains("+1 −0 lines"), "{r}");
        // A memory written after every change: unchanged.
        sh(&["commit", "-qam", "three"]);
        add(2, crate::db::now_ms() + 60_000);
        let r = report(&c, &t, 10).unwrap();
        let after = r.split("#2 ").nth(1).unwrap();
        assert!(after.lines().nth(1).unwrap().contains("unchanged"), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
