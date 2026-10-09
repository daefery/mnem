//! Moving an install of mnem (ravnori's name before 0.6.0) over to ravnori.
//!
//! What an old install left behind, and what `rvn install` does with it:
//! - `~/.mnem` (database, backups, settings, models, API token): moved to `~/.ravnori`
//!   when that does not exist yet; `~/.mnem` then becomes a link to it, so a running mnem
//!   or a script that names the old folder keeps working until it is replaced. `mnem.db`
//!   is renamed `ravnori.db` once nothing has it open (`db::database_path`).
//! - mnem's hooks in Claude Code and Codex: replaced by ravnori's (install treats
//!   `mnem hook` commands as its own; see `agents::is_hook_command`).
//! - the `mnem` MCP server in Claude Code and Codex: removed here; install adds `ravnori`.
//! - the pi extension `extensions/mnem`: removed here; install writes `extensions/ravnori`.
//! - the `mnem-watch` service (systemd or launchd): stopped and removed here; install
//!   starts `ravnori-watch`.
//! - the `mnem` command: replaced by a link to `rvn`, so `mnem doctor` keeps working for
//!   people and scripts that learned the old name. That link, the `MNEM_*` settings and
//!   the `~/.mnem` link are kept for one release (0.6.x) and dropped in 0.7.0.
//!
//! Every file is backed up before it changes, as install does. Running it again finds
//! nothing left to move.

use crate::db;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// ravnori's name before 0.6.0, in every place it was used.
pub const OLD: &str = "mnem";
/// Prefix of the old environment settings (`MNEM_HOME`, `MNEM_UI_PORT`...).
pub const OLD_ENV: &str = "MNEM";
/// The old database file name.
pub const OLD_DB: &str = "mnem.db";

/// Where an old install keeps its data (`MNEM_HOME`, else `~/.mnem`).
pub fn old_dir() -> PathBuf {
    std::env::var_os("MNEM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| db::home().join(".mnem"))
}

/// A mnem data folder: real (not the link left after the move) and holding a database,
/// under either name (a ravnori command run before install may have renamed it).
pub fn holds_data(dir: &Path) -> bool {
    !is_link(dir) && (dir.join(OLD_DB).exists() || dir.join("ravnori.db").exists())
}

/// `ravnori.db` in `dir`, renaming `mnem.db` to it first when nothing else has that open;
/// otherwise `mnem.db`, used where it is until a later run can rename it.
///
/// Whether anything else has it open: this connection checkpoints the write-ahead log
/// into the database and closes, and SQLite deletes the -wal file only when the last
/// connection closes. A -wal file still there means another process (an old mnem still
/// running) has the database open, and renaming it then could strand that process's
/// writes in a log nobody reads. A -wal file left by a process that was killed is
/// checkpointed and removed here, so it never blocks the rename.
pub fn adopt_database(dir: &Path) -> PathBuf {
    let new = dir.join("ravnori.db");
    let old = dir.join(OLD_DB);
    if new.exists() || !old.exists() {
        return new;
    }
    let side = |p: &Path, ext: &str| PathBuf::from(format!("{}-{ext}", p.display()));
    let checkpointed = rusqlite::Connection::open(&old)
        .and_then(|c| {
            c.busy_timeout(std::time::Duration::from_millis(500))?;
            c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .is_ok_and(|busy| busy == 0);
    if !checkpointed || side(&old, "wal").exists() {
        return old;
    }
    if std::fs::rename(&old, &new).is_err() {
        // Another process renamed it a moment ago.
        return if new.exists() { new } else { old };
    }
    let _ = std::fs::remove_file(side(&old, "shm"));
    new
}

fn is_link(p: &Path) -> bool {
    p.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Where ravnori's data goes: `RAVNORI_HOME`, else `~/.ravnori` (not `db::data_dir()`,
/// which still answers the old folder until it has moved).
fn new_dir() -> PathBuf {
    std::env::var_os("RAVNORI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| db::home().join(".ravnori"))
}

/// Move the data folder. Only when ravnori has none yet: an existing `~/.ravnori` is never
/// overwritten (both are reported, nothing is merged).
pub fn move_data(dry_run: bool) -> Result<Option<String>> {
    let old = old_dir();
    let new = new_dir();
    if !holds_data(&old) || old == new {
        return Ok(None);
    }
    if new.exists() {
        return Ok(Some(format!(
            "both {} and {} exist; ravnori uses {} and leaves {} as it is",
            old.display(),
            new.display(),
            new.display(),
            old.display()
        )));
    }
    if dry_run {
        return Ok(Some(format!(
            "would move {} to {}",
            old.display(),
            new.display()
        )));
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&old, &new)
        .with_context(|| format!("move {} to {}", old.display(), new.display()))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&new, &old)
        .with_context(|| format!("link {} to {}", old.display(), new.display()))?;
    Ok(Some(format!(
        "moved {} to {} (the old path links to the new one)",
        old.display(),
        new.display()
    )))
}

fn old_service_file() -> PathBuf {
    match crate::service::manager() {
        crate::service::Manager::Systemd => {
            db::home().join(".config/systemd/user/mnem-watch.service")
        }
        crate::service::Manager::Launchd => {
            db::home().join("Library/LaunchAgents/dev.mnem.watch.plist")
        }
    }
}

fn old_pi_extension() -> PathBuf {
    db::home().join(".pi/agent/extensions").join(OLD)
}

/// Whether anything from mnem is still here to move or take down.
pub fn needed() -> bool {
    holds_data(&old_dir()) || old_service_file().exists() || old_pi_extension().exists()
}

/// Stop and remove mnem's background service; install starts ravnori's.
pub fn remove_old_service(dry_run: bool) -> Result<Option<String>> {
    let file = old_service_file();
    if !file.exists() {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(format!("would stop and remove {}", file.display())));
    }
    let run = |cmd: &str, args: &[&str]| {
        let _ = std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    match crate::service::manager() {
        crate::service::Manager::Systemd => {
            run(
                "systemctl",
                &["--user", "disable", "--now", "mnem-watch.service"],
            );
            std::fs::remove_file(&file)?;
            run("systemctl", &["--user", "daemon-reload"]);
        }
        crate::service::Manager::Launchd => {
            #[cfg(unix)]
            let uid = unsafe { libc::getuid() };
            #[cfg(not(unix))]
            let uid = 0;
            run(
                "launchctl",
                &["bootout", &format!("gui/{uid}/dev.mnem.watch")],
            );
            std::fs::remove_file(&file)?;
        }
    }
    Ok(Some(format!("stopped and removed {}", file.display())))
}

/// Remove mnem's pi extension; install writes ravnori's.
pub fn remove_old_pi_extension(dry_run: bool) -> Result<Option<String>> {
    let ext = old_pi_extension();
    if !ext.exists() {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(format!("would remove {}", ext.display())));
    }
    std::fs::remove_dir_all(&ext)?;
    Ok(Some(format!("removed {}", ext.display())))
}

/// Whether a Claude Code state file or Codex config still registers the `mnem` server.
pub fn has_old_claude_mcp(state: &Path) -> bool {
    std::fs::read_to_string(state)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|d| d.get("mcpServers").and_then(|m| m.get(OLD)).is_some())
}

fn codex_config() -> PathBuf {
    db::home().join(".codex/config.toml")
}

pub fn has_old_codex_mcp() -> bool {
    std::fs::read_to_string(codex_config())
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .is_some_and(|d| d.get("mcp_servers").and_then(|m| m.get(OLD)).is_some())
}

/// Remove the `mnem` MCP server from Codex's config. Backs the file up first.
pub fn remove_old_codex_mcp(dry_run: bool) -> Result<Option<String>> {
    let cfg = codex_config();
    let Ok(cur) = std::fs::read_to_string(&cfg) else {
        return Ok(None);
    };
    let Ok(mut doc) = cur.parse::<toml_edit::DocumentMut>() else {
        return Ok(None);
    };
    let removed = doc
        .get_mut("mcp_servers")
        .and_then(|s| s.as_table_like_mut())
        .and_then(|s| s.remove(OLD))
        .is_some();
    if !removed {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(format!(
            "would remove [mcp_servers.mnem] from {}",
            cfg.display()
        )));
    }
    crate::install::backup_file(&cfg)?;
    std::fs::write(&cfg, doc.to_string())?;
    Ok(Some(format!(
        "removed [mcp_servers.mnem] from {}",
        cfg.display()
    )))
}

/// Remove the `mnem` MCP server from a Claude Code state file directly (used when the
/// `claude` command is not there, or left the entry in place).
pub fn remove_old_claude_mcp_file(state: &Path, dry_run: bool) -> Result<Option<String>> {
    let Ok(text) = std::fs::read_to_string(state) else {
        return Ok(None);
    };
    let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(None);
    };
    let removed = doc
        .get_mut("mcpServers")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.remove(OLD))
        .is_some();
    if !removed {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(format!(
            "would remove the mnem MCP server from {}",
            state.display()
        )));
    }
    crate::install::backup_file(state)?;
    std::fs::write(state, serde_json::to_string_pretty(&doc)? + "\n")?;
    Ok(Some(format!(
        "removed the mnem MCP server from {}",
        state.display()
    )))
}

/// The old command, beside the new one (`~/.local/bin/mnem` next to `~/.local/bin/rvn`)
/// or in `~/.cargo/bin` (from `cargo install`), becomes a link to `rvn`: `mnem ...`
/// keeps working for one release. Nothing is created where there was no old command.
pub fn link_old_command(bin: &str, dry_run: bool) -> Result<Vec<String>> {
    let rvn = Path::new(bin);
    let mut dirs: Vec<PathBuf> = rvn.parent().map(Path::to_path_buf).into_iter().collect();
    dirs.push(db::home().join(".cargo/bin"));
    dirs.dedup();
    let mut notes = Vec::new();
    for dir in dirs {
        let old = dir.join(OLD);
        let present = old.exists() || is_link(&old);
        let points_here = std::fs::read_link(&old).is_ok_and(|t| t == rvn);
        if !present || points_here {
            continue;
        }
        if dry_run {
            notes.push(format!("would make {} run rvn", old.display()));
            continue;
        }
        std::fs::remove_file(&old).with_context(|| format!("remove {}", old.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(rvn, &old)
            .with_context(|| format!("link {} to {}", old.display(), rvn.display()))?;
        notes.push(format!(
            "{} now runs rvn (the old command works until 0.7.0; use rvn)",
            old.display()
        ));
    }
    Ok(notes)
}

/// Run as `mnem` (the link left by the move): a one-line reminder on stderr, so scripts
/// that read stdout are not disturbed. Quiet for hooks and the MCP server, which agents
/// read and which `rvn install` rewrites anyway.
pub fn warn_if_old_name(arg0: Option<&std::ffi::OsStr>, quiet: bool) {
    let called = arg0
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|n| n.to_str());
    if called == Some(OLD) && !quiet {
        eprintln!("mnem is now ravnori: use `rvn` (the `mnem` command goes away in 0.7.0)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_database_is_renamed_only_when_nothing_has_it_open() {
        let d = crate::TempDir::new("rename-db");
        let old = d.join("mnem.db");
        // A killed process's leftovers: a database and its -wal still holding a write.
        let held = crate::db::open(&old).unwrap();
        held.execute_batch(
            "PRAGMA wal_autocheckpoint = 0; INSERT INTO meta(k, v) VALUES ('k', 'kept');",
        )
        .unwrap();
        // Open elsewhere: left as it is.
        assert_eq!(adopt_database(&d), old);
        assert!(old.exists());
        // What a killed process leaves: the same files, with no connection holding them.
        let crashed = crate::TempDir::new("rename-db-crashed");
        for f in ["mnem.db", "mnem.db-wal"] {
            std::fs::copy(d.join(f), crashed.join(f)).unwrap();
        }
        drop(held);
        // Adopted, and the write still in the -wal is not lost.
        let new = adopt_database(&crashed);
        assert_eq!(new, crashed.join("ravnori.db"));
        assert!(!crashed.join("mnem.db").exists() && !crashed.join("mnem.db-wal").exists());
        let v: String = crate::db::open(&new)
            .unwrap()
            .query_row("SELECT v FROM meta WHERE k = 'k'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "kept");
        // Already renamed: nothing to do.
        assert_eq!(adopt_database(&crashed), new);
    }

    #[test]
    fn the_claude_mcp_entry_is_removed_and_others_kept() {
        let d = crate::TempDir::new("rename-claude");
        let state = d.join(".claude.json");
        std::fs::write(
            &state,
            r#"{"mcpServers": {"mnem": {"command": "/x/mnem"}, "other": {"command": "o"}}, "theme": "dark"}"#,
        )
        .unwrap();
        assert!(has_old_claude_mcp(&state));
        assert!(remove_old_claude_mcp_file(&state, false).unwrap().is_some());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert!(v["mcpServers"].get("mnem").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "o");
        assert_eq!(v["theme"], "dark");
        // Nothing left: a second run changes nothing.
        assert!(!has_old_claude_mcp(&state));
        assert!(remove_old_claude_mcp_file(&state, false).unwrap().is_none());
    }
}
