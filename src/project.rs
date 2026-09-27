//! Project identity: the git remote when resolvable (so worktrees and clones of one repo
//! share memory), else the git root, else the raw cwd.

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
            let config = git_common_dir(&dotgit).map(|c| c.join("config"));
            if let Some(url) = config.and_then(|c| origin_url(&c)) {
                return Some(normalize_remote(&url));
            }
            return Some(d.to_string_lossy().into_owned());
        }
        dir = d.parent();
    }
    managed_worktree_remote(cwd)
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
    let gitdir = if gitdir.is_absolute() { gitdir } else { dotgit.parent()?.join(gitdir) };
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => Some(gitdir.join(c.trim())),
        Err(_) => Some(gitdir),
    }
}

fn origin_url(config: &Path) -> Option<String> {
    let s = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in s.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
        } else if in_origin && let Some(v) = line.strip_prefix("url") {
            return Some(v.trim_start().strip_prefix('=')?.trim().to_string());
        }
    }
    None
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
        Some((host, rest)) if !rest.starts_with(|c: char| c.is_ascii_digit()) => format!("{host}/{rest}"),
        _ => u.to_string(),
    }
    .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::normalize_remote;

    #[test]
    fn normalizes_remotes() {
        assert_eq!(normalize_remote("git@github.com:Org/Repo.git"), "github.com/org/repo");
        assert_eq!(normalize_remote("https://tok@gitlab.x.org/a/b.git"), "gitlab.x.org/a/b");
        assert_eq!(normalize_remote("https://github.com/a/b"), "github.com/a/b");
        assert_eq!(normalize_remote("ssh://git@host:2222/a/b.git"), "host:2222/a/b");
    }
}
