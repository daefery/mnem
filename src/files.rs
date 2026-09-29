//! File-aware recall: the memories about one file, and whether that file changed after
//! each memory was written.
//!
//! Memories list the files they read or modified (`memory_files`, kept by triggers).
//! Paths come in every shape, so a recorded path is placed before it is compared:
//! - relative, and the memory's session ran inside this repository: resolved against
//!   that session's directory (a monorepo package), then compared exactly;
//! - absolute inside this repository: compared exactly;
//! - otherwise (another machine, another checkout): the two paths must end alike at a
//!   directory boundary, and for an absolute path the repository's name must appear
//!   before that ending (`/Users/x/code/mnem/src/embed.rs` is mnem's `src/embed.rs`;
//!   `.../node_modules/pkg/extensions/index.ts` is not firstmate's). A lone file name
//!   only matches on its own.
//!
//! Only memories of the same project count. Staleness comes from git: the commits that
//! touched the file after the memory's session ended, uncommitted edits, and the size of
//! the change. A memory about a file that changed since may describe code that is gone.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Path components with `.`, `~` and empty parts dropped and `..` applied.
fn parts(p: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for c in p.split(['/', '\\']) {
        match c {
            "" | "." | "~" => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with('\\')
        || (b.len() > 2 && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\'))
}

/// Where a file is being looked up: its repo-relative path, the repository on this
/// machine when known, and names the repository goes by (its directory, its project).
pub struct Place<'a> {
    pub rel: &'a str,
    pub root: Option<&'a Path>,
    pub project: &'a str,
}

impl Place<'_> {
    fn names(&self) -> Vec<String> {
        let mut n = Vec::new();
        if let Some(r) = self.root.and_then(|r| r.file_name()) {
            n.push(r.to_string_lossy().to_lowercase());
        }
        // github.com/owner/repo#checkout: the repo and the checkout directory.
        let (repo, checkout) = self.project.split_once('#').unwrap_or((self.project, ""));
        if let Some(last) = repo.rsplit('/').next() {
            n.push(last.trim_end_matches(".git").to_lowercase());
        }
        if !checkout.is_empty() {
            n.push(checkout.to_lowercase());
        }
        n
    }
}

/// How a recorded path was matched: exactly (placed inside the repository, or the same
/// repo-relative path) or only by a shared ending (its context unknown).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Match {
    No,
    Exact,
    Ending,
}

/// Does a path recorded in a memory (whose session ran in `cwd`) name the file at
/// `place`?
pub fn names_file(recorded: &str, cwd: Option<&str>, place: &Place) -> bool {
    placement(recorded, cwd, place) != Match::No
}

pub fn placement(recorded: &str, cwd: Option<&str>, place: &Place) -> Match {
    let hit = |b: bool| if b { Match::Exact } else { Match::No };
    let want = parts(place.rel);
    let got = parts(recorded);
    if want.is_empty() || got.is_empty() {
        return Match::No;
    }
    let root = place.root.map(|r| r.to_string_lossy().replace('\\', "/"));
    let under_root = |p: &str| -> Option<Vec<String>> {
        let r = root.as_deref()?;
        let p = p.replace('\\', "/");
        p.strip_prefix(r)
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .map(|rest| parts(rest).iter().map(|s| s.to_string()).collect())
    };
    if is_absolute(recorded) {
        if let Some(inside) = under_root(recorded) {
            return hit(inside == want);
        }
        // Another machine or checkout: same ending, and the repository named before it.
        if got.len() < want.len() || !got.ends_with(&want) {
            return Match::No;
        }
        let names = place.names();
        return hit(got[..got.len() - want.len()]
            .iter()
            .any(|c| names.contains(&c.to_lowercase())));
    }
    // Relative to the session's directory, when that is inside this repository.
    if let Some(sub) = cwd.and_then(under_root) {
        let joined = format!("{}/{}", sub.join("/"), recorded);
        return hit(parts(&joined) == want);
    }
    // Otherwise a relative path may be relative to a package directory, so it can be
    // shorter than the repo-relative path and end the same way; a longer one names a
    // deeper, different file. A lone name only matches itself.
    if got.len() == 1 || want.len() == 1 || got.len() == want.len() {
        return hit(got == want);
    }
    if got.len() < want.len() && want.ends_with(&got) {
        Match::Ending
    } else {
        Match::No
    }
}

/// Files git tracks in `root`, as components (for telling whether a shared ending is
/// ambiguous). None when git cannot say.
fn tracked(root: &Path) -> Option<Vec<Vec<String>>> {
    let list = git(root, &["ls-files", "-z"])?;
    Some(
        list.split('\0')
            .filter(|f| !f.is_empty())
            .map(|f| parts(f).iter().map(|c| c.to_string()).collect())
            .collect(),
    )
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
    let place = Place {
        rel,
        root: None,
        project,
    };
    about_in(conn, &place, &crate::recall::Scope::default(), limit)
}

/// `about` within a recall scope: the asking session's own memories, memories already
/// offered to it, and memories created after `scope.before` are left out.
pub fn about_in(
    conn: &Connection,
    place: &Place,
    scope: &crate::recall::Scope,
    limit: usize,
) -> Result<Vec<About>> {
    let name = parts(place.rel)
        .last()
        .copied()
        .unwrap_or_default()
        .to_string();
    let mut st = conn.prepare_cached(
        "SELECT f.memory_id, f.modified, f.path, coalesce(m.type, m.kind), coalesce(m.title, ''),
                coalesce(m.created_at, 0), coalesce(min(s.last_event_at, m.created_at), m.created_at, 0),
                s.cwd
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
    // For matches by ending only: how many tracked files end that way (more than one
    // means the recorded path cannot tell which file it was), computed once.
    let mut listing: Option<Option<Vec<Vec<String>>>> = None;
    let rows = st.query_map(
        params![
            name,
            base(place.project),
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
                r.get::<_, Option<String>>(7)?,
            ))
        },
    )?;
    for row in rows {
        let (id, modified, path, kind, title, created_at, as_of, cwd) = row?;
        match placement(&path, cwd.as_deref(), place) {
            Match::No => continue,
            Match::Exact => {}
            // Only when exactly one tracked file ends that way; without a repository to
            // ask, or when git does not answer, the path cannot tell which file it was.
            Match::Ending => {
                let Some(root) = place.root else { continue };
                let Some(files) = listing.get_or_insert_with(|| tracked(root)) else {
                    continue;
                };
                let got: Vec<&str> = parts(&path);
                let n = files
                    .iter()
                    .filter(|f| {
                        f.len() >= got.len()
                            && f[f.len() - got.len()..]
                                .iter()
                                .zip(&got)
                                .all(|(a, b)| a == b)
                    })
                    .count();
                if n != 1 {
                    continue;
                }
            }
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
    /// Git answered both questions; without that nothing is known about changes.
    pub known: bool,
}

/// Longest any one git call may take; a stalled repository is treated as unknown.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Run git in `root` with file names taken literally (never as patterns), bounded by
/// GIT_TIMEOUT. None on failure or timeout.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait().ok()? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status.success().then_some(out)
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
    let log = git(root, &["log", "--format=%ct", "-n", "500", "--", rel]);
    let status = git(root, &["status", "--porcelain", "--", rel]);
    History {
        known: log.is_some() && status.is_some(),
        commits: log
            .map(|s| {
                s.lines()
                    .filter_map(|l| l.trim().parse::<i64>().ok().map(|t| t * 1000))
                    .collect()
            })
            .unwrap_or_default(),
        uncommitted: status.is_some_and(|s| !s.trim().is_empty()),
        exists: root.join(rel).exists(),
    }
}

/// Size of the change to a file since a memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lines {
    Counted(u64, u64),
    /// Git cannot count lines (a binary file).
    Binary,
}

/// Lines added and removed in the file since `as_of` (committed and not), from the
/// last commit before then to the working tree. None when there is no such commit.
pub fn lines_since(root: &Path, rel: &str, as_of: i64) -> Option<Lines> {
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
        match (f.next(), f.next()) {
            (Some("-"), _) | (_, Some("-")) => return Some(Lines::Binary),
            (Some(a), Some(r)) => {
                added += a.parse::<u64>().ok()?;
                removed += r.parse::<u64>().ok()?;
            }
            _ => return None,
        }
    }
    Some(Lines::Counted(added, removed))
}

/// Whether anything happened to the file after `as_of`.
fn changed(h: &History, as_of: i64) -> bool {
    !h.known || h.commits.iter().any(|t| *t > as_of) || h.uncommitted || !h.exists
}

/// How the file changed after `as_of`, in words; None when it did not.
pub fn change_since(h: &History, as_of: i64, now: i64, lines: Option<Lines>) -> Option<String> {
    let after: Vec<i64> = h.commits.iter().copied().filter(|t| *t > as_of).collect();
    let mut parts = Vec::new();
    if !h.exists {
        parts.push("the file no longer exists".to_string());
    }
    if !h.known {
        parts.push("history unknown (git did not answer in time)".to_string());
        return Some(parts.join(", "));
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
    if h.exists {
        match lines {
            Some(Lines::Counted(0, 0)) => parts.push("no net change in content".into()),
            Some(Lines::Counted(a, r)) => parts.push(format!("+{a} −{r} lines")),
            Some(Lines::Binary) => parts.push("binary content changed".into()),
            None => {}
        }
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

impl Target {
    pub fn place(&self) -> Place<'_> {
        Place {
            rel: &self.rel,
            root: Some(&self.root),
            project: &self.project,
        }
    }
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
/// memories about it (not its own session's, none offered before) and how the file
/// changed since them. Nothing is marked offered until the text is ready, so an
/// interrupted hook leaves the next touch to try again. None when there are none.
pub fn on_touch(
    conn: &Connection,
    session: &str,
    t: &Target,
    limit: usize,
) -> Result<Option<String>> {
    // Claim the file first: a parallel touch of it, or a retry after a run that timed
    // out, then shows nothing instead of the same memories again.
    if !claim(conn, session, t)? {
        return Ok(None);
    }
    let found = about_in(
        conn,
        &t.place(),
        &crate::recall::Scope::session(session),
        limit,
    )?;
    // Only memories this session has not been shown yet (by prompt recall or another
    // file), claimed one by one so parallel touches never show one twice.
    let tx = conn.unchecked_transaction()?;
    let mut fresh = Vec::new();
    for a in found {
        if tx.execute(
            "INSERT OR IGNORE INTO recall_seen(session_id, memory_id) VALUES (?1, ?2)",
            params![session, a.id],
        )? == 1
        {
            fresh.push(a);
        }
    }
    let ids: Vec<i64> = fresh.iter().map(|a| a.id).collect();
    // The text first (it reads transcripts and the file): a run cut short before it is
    // done leaves these memories unshown and free to be offered again.
    let text = (!fresh.is_empty()).then(|| touch_text(conn, t, &fresh));
    crate::uptake::offered(&tx, session, &ids, "file")?;
    tx.commit()?;
    Ok(text)
}

/// Mark `t` as touched by `session`; false when it already was.
pub fn claim(conn: &Connection, session: &str, t: &Target) -> Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO file_seen(session_id, path) VALUES (?1, ?2)",
        params![session, format!("{}:{}", t.project, t.rel)],
    )? == 1)
}

/// Compact: one line per memory with whether its own edits are still in the file (or,
/// when they cannot be read back, the size of the change since it), and one line saying
/// how the file moved on since the newest memory that may be out of date.
fn touch_text(conn: &Connection, t: &Target, found: &[About]) -> String {
    let h = history(&t.root, &t.rel);
    let now = crate::db::now_ms();
    let mut w = format!(
        "mnem: past memories about {} (full text: get_observations([ids]))\n",
        t.rel
    );
    let mut stale = Vec::new();
    for a in found {
        let kept = a
            .modified
            .then(|| edits_kept(conn, a.id, &t.root, &t.rel).ok().flatten())
            .flatten();
        let note = if let Some(k) = kept.filter(|_| h.exists) {
            if !k.intact() {
                stale.push(a.as_of);
            }
            format!(" ({})", k.phrase())
        } else if changed(&h, a.as_of) {
            stale.push(a.as_of);
            // Counting lines is one more git call: only when git answered and the file is there.
            let lines = (h.exists && h.known)
                .then(|| lines_since(&t.root, &t.rel, a.as_of))
                .flatten();
            match (h.exists, lines) {
                (false, _) => " (file gone since)".to_string(),
                _ if !h.known => " (file history unknown)".to_string(),
                (true, Some(Lines::Counted(0, 0))) => " (no net change since)".to_string(),
                (true, Some(Lines::Counted(x, y))) => format!(" (file since: +{x} −{y} lines)"),
                (true, Some(Lines::Binary)) => " (binary content changed since)".to_string(),
                (true, None) => " (file changed since)".to_string(),
            }
        } else {
            " (file unchanged since)".to_string()
        };
        w.push_str(&format!(
            "#{} {} · {} · {}{note}\n",
            a.id,
            a.kind,
            crate::context::ago(now - a.created_at),
            crate::text::head(&a.title, 110)
        ));
    }
    if let Some(newest) = stale.iter().max() {
        let summary = change_since(&h, *newest, now, None)
            .unwrap_or_else(|| "it changed since some of them".into());
        w.push_str(&format!(
            "{} of {} may describe code that changed: {summary}. Check the code before relying on them.\n",
            stale.len(),
            found.len()
        ));
    }
    w
}

/// Memories about a file with how it changed since each, as text for an agent.
pub fn report(conn: &Connection, t: &Target, limit: usize) -> Result<String> {
    let found = about_in(conn, &t.place(), &crate::recall::Scope::default(), limit)?;
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
        let kept = a
            .modified
            .then(|| edits_kept(conn, a.id, &t.root, &t.rel).ok().flatten())
            .flatten()
            .filter(|_| h.exists);
        let lines = changed(&h, a.as_of)
            .then(|| lines_since(&t.root, &t.rel, a.as_of))
            .flatten();
        let file = change_since(&h, a.as_of, now, lines);
        stale += (file.is_some() && !kept.is_some_and(|k| k.intact())) as usize;
        // One line: whether its own edits survive, then how the file moved on.
        w.push_str(&match (kept, file) {
            (Some(k), Some(c)) => format!("   {} (file changed since: {c})\n", k.phrase()),
            (Some(k), None) => format!("   {} (file unchanged since)\n", k.phrase()),
            (None, Some(c)) => format!("   changed since: {c}\n"),
            (None, None) => "   file unchanged since\n".to_string(),
        });
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
            match edits_kept(conn, memory_id, &root, &rel)?.filter(|_| h.exists) {
                Some(k) => out.push(format!("{rel}: {} (file: {c})", k.phrase())),
                None => out.push(format!("{rel}: {c}")),
            }
        }
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

/// How much of what a memory's session left in a file is still in it: lines kept and
/// lines looked for. None when that cannot be told reliably, and the file-level note is
/// used instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Kept {
    pub kept: usize,
    pub of: usize,
}

impl Kept {
    /// Most of it (4 in 5 lines or more) is still there.
    pub fn intact(&self) -> bool {
        self.kept * 5 >= self.of * 4
    }

    pub fn phrase(&self) -> String {
        if self.intact() {
            "its edited lines are still there".into()
        } else if self.kept * 5 >= self.of {
            format!(
                "its edited lines partly changed, {} of {} kept",
                self.kept, self.of
            )
        } else {
            "its edited lines are gone".into()
        }
    }
}

/// Bounds that keep the check inside a hook's time: edits read back per memory and
/// file (more than this and the chunk's final state is not reconstructed), bytes of one
/// transcript record, bytes of the current file, and lines compared.
const EDITS_READ: usize = 20;
const RECORD_BYTES: u64 = 4 << 20;
const FILE_BYTES: u64 = 2 << 20;
const LINES_COMPARED: usize = 400;

/// One edit as the agent made it: the lines it replaced and the lines it wrote,
/// trimmed, 12 characters or more (braces and blank lines say nothing).
#[derive(Debug, Default, PartialEq)]
pub struct Edit {
    pub old: Vec<String>,
    pub new: Vec<String>,
}

/// Whether a recorded path is `rel` (repo-relative): the same path, or one ending in
/// `/rel` (never a longer file name ending in the same characters).
fn is_rel(recorded: &str, rel: &str) -> bool {
    let p = recorded.replace('\\', "/");
    p == rel || p.ends_with(&format!("/{rel}"))
}

/// The lines memory `id`'s session left in `rel` (under `root`), compared with the file
/// as it is now. Events keep only an edit's path, so each edit is read back from the
/// transcript record it came from and checked to be that event's tool call. Edits apply
/// in order within the memory's chunk: a line a later edit replaced is not counted.
/// Only lines that occur at most once in the file now are evidence (a line found in
/// several places says nothing about this one).
pub fn edits_kept(conn: &Connection, id: i64, root: &Path, rel: &str) -> Result<Option<Kept>> {
    let origin: Option<(String, String)> = conn
        .query_row(
            "SELECT coalesce(session_id, ''), coalesce(origin_id, '') FROM memories WHERE id = ?1 AND origin = 'mnem'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    // Distilled memories are named `<session>@<first event>-<last event>#<n>`.
    let Some((from, through, session)) = origin.and_then(|(session, o)| {
        let range = o.rsplit_once('@')?.1.split('#').next()?;
        let (a, b) = range.split_once('-')?;
        Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?, session))
    }) else {
        return Ok(None);
    };
    let mut st = conn.prepare_cached(
        "SELECT path, record_key, source_path, coalesce(byte_offset, 0) FROM events
          WHERE session_id = ?1 AND id BETWEEN ?2 AND ?3 AND kind = 'file_edit'
            AND path IS NOT NULL AND source_path IS NOT NULL ORDER BY id",
    )?;
    let edits: Vec<(String, String, String, i64)> = st
        .query_map(params![session, from, through], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter(|e| is_rel(&e.0, rel))
        .take(EDITS_READ + 1)
        .collect();
    if edits.is_empty() || edits.len() > EDITS_READ {
        return Ok(None);
    }
    // The chunk's edits in order: what a later one replaced no longer counts.
    let mut left: Vec<String> = Vec::new();
    for (path, key, source, offset) in &edits {
        let Some(e) = record_at(Path::new(source), *offset).and_then(|r| edit_at(&r, key, path))
        else {
            // One edit that cannot be read back leaves the final state unknown.
            return Ok(None);
        };
        left.retain(|l| !e.old.contains(l));
        for l in e.new {
            if !left.contains(&l) {
                left.push(l);
            }
        }
    }
    let file = root.join(rel);
    if !std::fs::metadata(&file).is_ok_and(|m| m.is_file() && m.len() <= FILE_BYTES) {
        return Ok(None);
    }
    let Ok(now) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    let mut count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for l in now.lines() {
        *count.entry(l.trim()).or_default() += 1;
    }
    let evidence: Vec<usize> = left
        .iter()
        .take(LINES_COMPARED)
        .map(|l| count.get(l.as_str()).copied().unwrap_or(0))
        .filter(|n| *n <= 1)
        .collect();
    if evidence.is_empty() {
        return Ok(None);
    }
    Ok(Some(Kept {
        kept: evidence.iter().filter(|n| **n == 1).count(),
        of: evidence.len(),
    }))
}

/// The transcript record (one JSON line) at `offset` in `file`: a regular file, and a
/// line of at most RECORD_BYTES.
fn record_at(file: &Path, offset: i64) -> Option<serde_json::Value> {
    use std::io::{BufRead, Read, Seek, SeekFrom};
    if !std::fs::metadata(file).ok()?.is_file() {
        return None;
    }
    let mut f = std::fs::File::open(file).ok()?;
    f.seek(SeekFrom::Start(u64::try_from(offset).ok()?)).ok()?;
    let mut line = Vec::new();
    std::io::BufReader::new(f.take(RECORD_BYTES))
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.last() != Some(&b'\n') && line.len() as u64 >= RECORD_BYTES {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

/// The edit event `key` (`<record id>:<content index>` for Claude Code and pi,
/// `<item id>:<path hash>` for Codex) made to `path`, if `record` is the record it came
/// from. A record at that offset with another id (a transcript rewritten since) is None.
pub fn edit_at(record: &serde_json::Value, key: &str, path: &str) -> Option<Edit> {
    use serde_json::Value;
    let (id, part) = key.rsplit_once(':')?;
    let lines = |t: &str, out: &mut Vec<String>| {
        out.extend(
            t.lines()
                .map(str::trim)
                .filter(|l| l.chars().count() >= 12)
                .map(str::to_string),
        )
    };
    fn texts(input: &Value, keys: &[&str], out: &mut Vec<String>) {
        for k in keys {
            if let Some(t) = input.get(*k).and_then(Value::as_str) {
                out.push(t.to_string());
            }
        }
        for e in input.get("edits").and_then(Value::as_array).into_iter().flatten() {
            texts(e, keys, out);
        }
    }
    let same = |a: &str| a.replace('\\', "/") == path.replace('\\', "/");
    let str_at = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    // Claude Code (`uuid`) and pi (`id`): the tool call at that index of the message.
    if str_at(record, "uuid").or_else(|| str_at(record, "id")).as_deref() == Some(id) {
        let call = record.get("message")?.get("content")?.get(part.parse::<usize>().ok()?)?;
        let input = match call.get("input").or_else(|| call.get("arguments"))? {
            Value::String(s) => serde_json::from_str::<Value>(s).ok()?,
            v => v.clone(),
        };
        let named = ["file_path", "path", "notebook_path"]
            .iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))?;
        if !same(named) {
            return None;
        }
        let (mut old, mut new) = (Vec::new(), Vec::new());
        texts(&input, &["old_string", "oldText", "old_str"], &mut old);
        texts(&input, &["new_string", "newText", "new_str", "content"], &mut new);
        let mut e = Edit::default();
        old.iter().for_each(|t| lines(t, &mut e.old));
        new.iter().for_each(|t| lines(t, &mut e.new));
        return Some(e);
    }
    // Codex: the item with that id, its change to this path (a whole file or a diff).
    let item = record
        .get("payload")
        .and_then(|p| p.get("item"))
        .or_else(|| record.get("item"))?;
    if str_at(item, "id").as_deref() != Some(id) {
        return None;
    }
    let changes = item.get("changes")?.as_object()?;
    let content = changes
        .iter()
        .find(|(p, _)| same(p))?
        .1
        .get("content")?
        .as_str()?;
    let mut e = Edit::default();
    if content.lines().any(|l| l.starts_with("@@")) {
        for l in content.lines() {
            if let Some(t) = l.strip_prefix('+').filter(|_| !l.starts_with("+++")) {
                lines(t, &mut e.new);
            } else if let Some(t) = l.strip_prefix('-').filter(|_| !l.starts_with("---")) {
                lines(t, &mut e.old);
            }
        }
    } else {
        lines(content, &mut e.new);
    }
    Some(e)
}

/// A recorded path as a repo-relative path under `root`, if that file exists here.
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

    fn at<'a>(rel: &'a str, root: Option<&'a Path>, project: &'a str) -> Place<'a> {
        Place { rel, root, project }
    }

    #[test]
    fn recorded_paths_are_placed_before_they_are_compared() {
        let root = Path::new("/home/me/code/mnem");
        let p = at("src/embed.rs", Some(root), "github.com/daefery/mnem");
        assert!(names_file("src/embed.rs", None, &p));
        assert!(names_file("./src/embed.rs", None, &p));
        assert!(names_file("/home/me/code/mnem/src/embed.rs", None, &p));
        assert!(names_file("src\\embed.rs", None, &p));
        // Another machine: the repository's name is in the path.
        assert!(names_file("/Users/x/code/mnem/src/embed.rs", None, &p));
        // Another project's file with the same ending is not this one.
        assert!(!names_file(
            "/home/me/.pi/node_modules/pkg/src/embed.rs",
            None,
            &p
        ));
        assert!(!names_file("src/embedder.rs", None, &p));
        assert!(!names_file("/home/me/code/mnem/lib/src/embed.rs", None, &p));
        // A lone name matches only itself.
        let readme = at("README.md", Some(root), "github.com/daefery/mnem");
        assert!(names_file("README.md", None, &readme));
        assert!(!names_file("docs/README.md", None, &readme));
        assert!(names_file("/Users/x/code/mnem/README.md", None, &readme));
        assert!(!names_file("/Users/x/other/README.md", None, &readme));
        // `..` is applied, not matched literally.
        assert!(names_file("docs/../src/embed.rs", None, &p));
    }

    #[test]
    fn package_relative_paths_resolve_through_the_sessions_directory() {
        let root = Path::new("/r");
        let a = at("packages/a/src/shared.rs", Some(root), "github.com/o/r");
        let b = at("packages/b/src/shared.rs", Some(root), "github.com/o/r");
        assert!(names_file("src/shared.rs", Some("/r/packages/a"), &a));
        assert!(!names_file("src/shared.rs", Some("/r/packages/a"), &b));
        // A session at the root recorded repo-relative paths.
        assert!(names_file("packages/b/src/shared.rs", Some("/r"), &b));
        // Session directory unknown here: a shared ending (verified against the
        // repository's files before it is trusted).
        assert_eq!(placement("src/shared.rs", None, &b), Match::Ending);
        // A longer relative path is a deeper, different file.
        let short = at("extensions/index.ts", Some(root), "github.com/o/r");
        assert!(!names_file(
            "extensions/@scope/pkg/extensions/index.ts",
            None,
            &short
        ));
    }

    #[test]
    fn change_is_counted_after_the_memory_only() {
        let h = History {
            commits: vec![3_000, 2_000, 1_000],
            uncommitted: false,
            exists: true,
            known: true,
        };
        assert_eq!(change_since(&h, 3_000, 10_000, None), None);
        assert_eq!(
            change_since(&h, 1_500, 10_000, Some(Lines::Counted(12, 3))).unwrap(),
            "2 commits (latest 7s ago), +12 −3 lines"
        );
        assert!(
            change_since(&h, 1_500, 10_000, Some(Lines::Binary))
                .unwrap()
                .ends_with("binary content changed")
        );
        let gone = History {
            exists: false,
            known: true,
            ..Default::default()
        };
        let unknown = History::default();
        assert!(
            change_since(&unknown, 0, 0, None)
                .unwrap()
                .contains("history unknown")
        );
        assert_eq!(
            change_since(&gone, 0, 0, Some(Lines::Counted(0, 0))).unwrap(),
            "the file no longer exists"
        );
    }

    #[test]
    fn about_finds_memories_of_the_project_only() {
        let c = crate::db::open_with(Path::new(":memory:"), Duration::from_secs(1)).unwrap();
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
        add(6, "github.com/a/r", r#"["C:\\work\\r\\src\\embed.rs"]"#, 60);
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
        // Modifiers first, newest first; the other project and the other file are out;
        // a Windows path is indexed like any other.
        assert_eq!(found, vec![(6, true), (2, true), (1, true), (5, false)]);
        // The index follows edits and deletes.
        c.execute(
            "UPDATE memories SET files_modified = '[]' WHERE id IN (2, 6)",
            [],
        )
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
    fn git_treats_file_names_literally() {
        let dir = std::env::temp_dir().join(format!("mnem-files-lit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
        };
        sh(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        std::fs::write(dir.join("[a].txt"), "x\n").unwrap();
        sh(&["add", "."]);
        sh(&["commit", "-qm", "one"]);
        std::fs::write(dir.join("a.txt"), "y\n").unwrap();
        // `[a].txt` is a pattern that matches a.txt unless names are literal.
        assert!(!history(&dir, "[a].txt").uncommitted);
        assert!(history(&dir, "a.txt").uncommitted);
        let _ = std::fs::remove_dir_all(&dir);
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
        let c = crate::db::open_with(Path::new(":memory:"), Duration::from_secs(1)).unwrap();
        let add = |id: i64, created: i64| {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, files_modified, origin, origin_id, created_at)
                 VALUES (?1, ?2, 'observation', 'bugfix', ?3, '[\"src/a.rs\"]', 'mnem', ?1, ?4)",
                params![id, t.project, format!("memory {id}"), created],
            )
            .unwrap();
        };
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
        // The touch text summarises once instead of repeating on every line.
        let touch = on_touch(&c, "claude:s", &t, 3).unwrap().unwrap();
        assert!(touch.contains("(file since: +1 −0 lines)"), "{touch}");
        assert!(
            touch.contains("1 of 1 may describe code that changed: "),
            "{touch}"
        );
        assert!(
            on_touch(&c, "claude:s", &t, 3).unwrap().is_none(),
            "once per file"
        );
        // A memory written after every change: unchanged.
        sh(&["commit", "-qam", "three"]);
        add(2, crate::db::now_ms() + 60_000);
        let r = report(&c, &t, 10).unwrap();
        let after = r.split("#2 ").nth(1).unwrap();
        assert!(after.lines().nth(1).unwrap().contains("unchanged"), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_is_read_back_from_its_own_tool_call_only() {
        use serde_json::json;
        let line = "let retries = backoff(attempt);";
        let other = "let unrelated = something_else();";
        // Claude Code: the record's uuid, the tool call at that content index.
        let claude = json!({ "uuid": "u1", "message": { "content": [
            { "type": "tool_use", "input": { "file_path": "/r/src/a.rs", "old_string": other, "new_string": other } },
            { "type": "tool_use", "input": { "file_path": "/r/src/a.rs", "old_string": "fn old_name_here() {}", "new_string": format!("{line}\n}}") } },
        ] } });
        let e = edit_at(&claude, "u1:1", "/r/src/a.rs").unwrap();
        assert_eq!((e.old, e.new), (vec!["fn old_name_here() {}".to_string()], vec![line.to_string()]));
        // A rewritten transcript: another record at that offset is not this edit.
        assert!(edit_at(&claude, "u2:1", "/r/src/a.rs").is_none());
        // The event's path must be the tool call's path.
        assert!(edit_at(&claude, "u1:1", "/r/src/b.rs").is_none());
        // pi: the record's id, arguments as an object or a JSON string, and edit lists.
        let pi = json!({ "id": "p1", "message": { "content": [{ "type": "toolCall",
            "arguments": json!({ "path": "src/a.rs", "edits": [{ "oldText": other, "newText": line }] }).to_string() }] } });
        let e = edit_at(&pi, "p1:0", "src/a.rs").unwrap();
        assert_eq!((e.old, e.new), (vec![other.to_string()], vec![line.to_string()]));
        // Codex: the item's id, its change to this path; a diff's added and removed lines.
        let codex = json!({ "payload": { "item": { "id": "c1", "changes": { "/r/src/a.rs": {
            "type": "update", "content": format!("@@ -1 +1 @@\n-fn old_name_here() {{}}\n+{line}\n") } } } } });
        let e = edit_at(&codex, "c1:abc", "/r/src/a.rs").unwrap();
        assert_eq!((e.old, e.new), (vec!["fn old_name_here() {}".to_string()], vec![line.to_string()]));
        assert!(edit_at(&codex, "c2:abc", "/r/src/a.rs").is_none());
        // A path matches on whole components only.
        assert!(is_rel("/r/src/a.rs", "src/a.rs") && is_rel("src\\a.rs", "src/a.rs"));
        assert!(!is_rel("/r/src/not_a.rs", "a.rs"));
    }

    #[test]
    fn a_memory_says_whether_its_own_edits_are_still_there() {
        let dir = std::env::temp_dir().join(format!("mnem-files-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let file = dir.join("src/a.rs").to_string_lossy().to_string();
        let kept = "let retries = backoff(attempt);";
        let gone = "log::warn!(\"giving up after {n} tries\");";
        let first = "let first_draft = compute_once();";
        let twice = "let repeated_line = shared_helper();";
        // The transcript: one Claude Code record per edit.
        let transcript = dir.join("t.jsonl");
        let mut body = String::new();
        let mut offsets = Vec::new();
        for (i, (old, new)) in [
            ("", kept),        // event 10
            ("", gone),        // event 11
            ("", first),       // event 12: replaced by event 13 in the same chunk
            (first, kept),     // event 13
            ("", twice),       // event 14
        ]
        .into_iter()
        .enumerate()
        {
            offsets.push(body.len() as i64);
            body.push_str(&serde_json::json!({ "uuid": format!("u{i}"), "message": { "content": [
                { "type": "tool_use", "input": { "file_path": file, "old_string": old, "new_string": new } }] } })
                .to_string());
            body.push('\n');
        }
        std::fs::write(&transcript, &body).unwrap();
        let c = crate::db::open_with(Path::new(":memory:"), Duration::from_secs(1)).unwrap();
        for (i, off) in offsets.iter().enumerate() {
            c.execute(
                "INSERT INTO events(id, session_id, record_key, kind, path, source_path, byte_offset)
                 VALUES (?1, 's', ?2, 'file_edit', ?3, ?4, ?5)",
                params![10 + i as i64, format!("u{i}:0"), file, transcript.to_string_lossy(), off],
            )
            .unwrap();
        }
        // Another file whose name ends the same way: never this file's edit.
        c.execute(
            "INSERT INTO events(id, session_id, record_key, kind, path, source_path, byte_offset)
             VALUES (15, 's', 'u9:0', 'file_edit', '/elsewhere/src/not_a.rs', ?1, 0)",
            [transcript.to_string_lossy()],
        )
        .unwrap();
        let memory = |id: i64, range: &str| {
            c.execute(
                "INSERT INTO memories(id, session_id, project, kind, title, origin, origin_id) VALUES (?1, 's', 'p', 'observation', 't', 'mnem', ?2)",
                params![id, format!("s@{range}#0")],
            )
            .unwrap();
        };
        memory(1, "10-10");
        memory(2, "11-11");
        memory(3, "12-13");
        memory(4, "14-14");
        memory(5, "16-20");
        memory(6, "15-15");
        std::fs::write(
            dir.join("src/a.rs"),
            format!("fn new() {{}}\n    {kept}\n// {gone}\n{twice}\n{twice}\n"),
        )
        .unwrap();
        let k = |id| edits_kept(&c, id, &dir, "src/a.rs").unwrap();
        assert_eq!(k(1), Some(Kept { kept: 1, of: 1 }));
        assert_eq!(k(1).unwrap().phrase(), "its edited lines are still there");
        // Commented out is not still there.
        assert_eq!(k(2).unwrap().phrase(), "its edited lines are gone");
        // The first draft was replaced within the chunk: only what it left counts.
        assert_eq!(k(3), Some(Kept { kept: 1, of: 1 }));
        // A line found twice in the file now is no evidence either way.
        assert_eq!(k(4), None);
        assert_eq!(k(5), None, "no edits in its range");
        assert_eq!(k(6), None, "not_a.rs is not a.rs");
        // A transcript rewritten since: the record at that offset is another one.
        std::fs::write(&transcript, body.replace("\"u0\"", "\"x0\"")).unwrap();
        assert_eq!(k(1), None);
        assert_eq!(
            Kept { kept: 3, of: 10 }.phrase(),
            "its edited lines partly changed, 3 of 10 kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ending_that_fits_several_tracked_files_does_not_match() {
        let dir = std::env::temp_dir().join(format!("mnem-files-amb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for p in ["packages/a/src", "packages/b/src", "packages/c/lib"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        std::fs::write(dir.join("packages/a/src/shared.rs"), "a\n").unwrap();
        std::fs::write(dir.join("packages/b/src/shared.rs"), "b\n").unwrap();
        std::fs::write(dir.join("packages/c/lib/only.rs"), "c\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);
        let c = crate::db::open_with(Path::new(":memory:"), Duration::from_secs(1)).unwrap();
        for (id, file) in [(1, "src/shared.rs"), (2, "lib/only.rs")] {
            c.execute(
                "INSERT INTO memories(id, project, kind, type, title, files_modified, origin, origin_id, created_at)
                 VALUES (?1, 'p', 'observation', 'bugfix', 't', ?2, 'mnem', ?1, 1)",
                params![id, format!("[\"{file}\"]")],
            )
            .unwrap();
        }
        let root = dir.canonicalize().unwrap();
        let ids = |rel: &str| -> Vec<i64> {
            let place = Place {
                rel,
                root: Some(&root),
                project: "p",
            };
            about_in(&c, &place, &crate::recall::Scope::default(), 10)
                .unwrap()
                .iter()
                .map(|a| a.id)
                .collect()
        };
        // Which package's src/shared.rs? Unknown: neither gets the memory.
        assert!(ids("packages/a/src/shared.rs").is_empty());
        assert!(ids("packages/b/src/shared.rs").is_empty());
        // Only one tracked file ends in lib/only.rs.
        assert_eq!(ids("packages/c/lib/only.rs"), vec![2]);
        // Without a repository to check against, an ending alone is not enough.
        let place = Place {
            rel: "packages/c/lib/only.rs",
            root: None,
            project: "p",
        };
        assert!(
            about_in(&c, &place, &crate::recall::Scope::default(), 10)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
