//! Project identity: the git remote when resolvable (so worktrees of one checkout share
//! memory), else the git root, else the raw cwd.
//!
//! Two separate checkouts of the same remote are often different projects (a fork used
//! for other work, a second clone with its own purpose). When the main checkout's
//! directory name differs from the repo name, it is appended: `github.com/o/repo#dir`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Resolver {
    cache: HashMap<String, String>,
}

impl Resolver {
    pub fn resolve(&mut self, cwd: Option<&str>, repo_url: Option<&str>) -> Option<String> {
        let Some(cwd) = cwd else {
            return repo_url.map(normalize_remote);
        };
        if let Some(p) = self.cache.get(cwd) {
            return Some(p.clone());
        }
        let p = resolve_uncached(Path::new(cwd))
            .or_else(|| repo_url.map(normalize_remote))
            .unwrap_or_else(|| cwd.to_string());
        self.cache.insert(cwd.to_string(), p.clone());
        Some(p)
    }
}

fn resolve_uncached(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dotgit = d.join(".git");
        if dotgit.exists() {
            let common = git_common_dir(&dotgit);
            if let Some(url) = common.as_ref().and_then(|c| origin_url(&c.join("config"))) {
                let remote = normalize_remote(&url);
                return Some(match common.as_deref().and_then(checkout_name) {
                    Some(name)
                        if !remote
                            .rsplit('/')
                            .next()
                            .is_some_and(|r| r.eq_ignore_ascii_case(&name)) =>
                    {
                        format!("{remote}#{}", name.to_lowercase())
                    }
                    _ => remote,
                });
            }
            return Some(d.to_string_lossy().into_owned());
        }
        dir = d.parent();
    }
    managed_worktree_remote(cwd)
}

/// Directory name of the main working checkout that owns `common` (a `.git` dir).
/// None for bare repositories, whose names are often ids rather than project names.
fn checkout_name(common: &Path) -> Option<String> {
    let common = std::fs::canonicalize(common).unwrap_or_else(|_| common.to_path_buf());
    if common.file_name()? != ".git" {
        return None;
    }
    Some(common.parent()?.file_name()?.to_string_lossy().into_owned())
}

/// Tools like no-mistakes keep `<root>/repos/<id>.git` and check out throwaway
/// worktrees under `<root>/worktrees/<id>/<run>`. Those worktrees are usually deleted
/// by the time we backfill, but the bare repo still knows the remote.
fn managed_worktree_remote(cwd: &Path) -> Option<String> {
    let parts: Vec<_> = cwd.components().collect();
    let i = parts.iter().rposition(|c| c.as_os_str() == "worktrees")?;
    let id = parts.get(i + 1)?.as_os_str().to_str()?;
    let root: PathBuf = parts[..i].iter().collect();
    let config = root.join("repos").join(format!("{id}.git")).join("config");
    origin_url(&config).map(|u| normalize_remote(&u))
}

/// `.git` is a directory for normal clones, or a `gitdir: <path>` file for worktrees.
fn git_common_dir(dotgit: &Path) -> Option<PathBuf> {
    if dotgit.is_dir() {
        return Some(dotgit.to_path_buf());
    }
    let s = std::fs::read_to_string(dotgit).ok()?;
    let gitdir = PathBuf::from(s.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        dotgit.parent()?.join(gitdir)
    };
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => Some(gitdir.join(c.trim())),
        Err(_) => Some(gitdir),
    }
}

/// Remote URL from a git config: `origin` when present, else the first remote.
fn origin_url(config: &Path) -> Option<String> {
    let s = std::fs::read_to_string(config).ok()?;
    let mut remote: Option<&str> = None;
    let (mut origin, mut first) = (None, None);
    for line in s.lines().map(str::trim) {
        if line.starts_with('[') {
            remote = line
                .strip_prefix("[remote \"")
                .and_then(|r| r.strip_suffix("\"]"));
        } else if let Some(name) = remote
            && let Some(v) = line.strip_prefix("url")
            && let Some(url) = v.trim_start().strip_prefix('=')
        {
            let url = url.trim().to_string();
            if name == "origin" {
                origin = Some(url);
            } else if first.is_none() {
                first = Some(url);
            }
        }
    }
    origin.or(first)
}

/// git@github.com:a/b.git, https://user@github.com/a/b.git -> github.com/a/b
pub fn normalize_remote(url: &str) -> String {
    let mut u = url.trim();
    if let Some(i) = u.find("://") {
        u = &u[i + 3..];
    }
    if let Some(i) = u.find('@')
        && !u[..i].contains('/')
    {
        u = &u[i + 1..];
    }
    let u = u.trim_end_matches('/').trim_end_matches(".git");
    match u.split_once(':') {
        // scp-like syntax; leave host:port/path alone.
        Some((host, rest)) if !rest.starts_with(|c: char| c.is_ascii_digit()) => {
            format!("{host}/{rest}")
        }
        _ => u.to_string(),
    }
    .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{Resolver, normalize_remote};

    #[test]
    fn second_checkout_of_same_remote_is_its_own_project() {
        let root = crate::TempDir::new("proj");
        for dir in ["firstmate", "secondmate"] {
            let git = root.join(dir).join(".git");
            std::fs::create_dir_all(&git).unwrap();
            std::fs::write(
                git.join("config"),
                "[remote \"origin\"]\n\turl = git@github.com:o/firstmate.git\n",
            )
            .unwrap();
        }
        let mut r = Resolver::default();
        let p = |d: &str| root.join(d).join("src").to_string_lossy().into_owned();
        std::fs::create_dir_all(root.join("firstmate/src")).unwrap();
        std::fs::create_dir_all(root.join("secondmate/src")).unwrap();
        assert_eq!(
            r.resolve(Some(&p("firstmate")), None).unwrap(),
            "github.com/o/firstmate"
        );
        assert_eq!(
            r.resolve(Some(&p("secondmate")), None).unwrap(),
            "github.com/o/firstmate#secondmate"
        );
    }

    #[test]
    fn normalizes_remotes() {
        assert_eq!(
            normalize_remote("git@github.com:Org/Repo.git"),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize_remote("https://tok@gitlab.x.org/a/b.git"),
            "gitlab.x.org/a/b"
        );
        assert_eq!(normalize_remote("https://github.com/a/b"), "github.com/a/b");
        assert_eq!(
            normalize_remote("ssh://git@host:2222/a/b.git"),
            "host:2222/a/b"
        );
    }
}
