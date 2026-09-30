//! Backups: consistent snapshots of the database, verified before they are trusted.
//!
//! A snapshot is written with `VACUUM INTO` (consistent even while hooks and the
//! watcher write), then reopened and checked: `integrity_check`, schema version, row
//! counts and a SHA-256, recorded in a `.json` manifest beside it. A backup that was
//! never verified is not counted as a backup.
//!
//! A snapshot also records where it came from and the settings in use (config.json,
//! which names key files but never holds keys), so one file moves mnem to a new
//! machine: copy it over, import it in the viewer or with `mnem restore --apply`.

use crate::db;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const KEEP: usize = 7;
/// How long restore waits for other writers before giving up without changes.
const RESTORE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// The watcher takes a new snapshot when the newest one is older than this.
pub const INTERVAL_MS: i64 = 24 * 3_600_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub file: String,
    pub created_at: i64,
    pub bytes: u64,
    pub sha256: String,
    pub schema_version: i64,
    pub sessions: i64,
    pub events: i64,
    pub memories: i64,
}

pub fn dir() -> PathBuf {
    db::data_dir().join("backups")
}

fn manifest_path(snapshot: &Path) -> PathBuf {
    snapshot.with_extension("json")
}

fn sha256(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Open a snapshot read-only and check it is a sound mnem database.
pub fn inspect(path: &Path) -> Result<Manifest> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", path.display()))?;
    let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(ok == "ok", "integrity_check failed: {ok}");
    let schema_version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let count = |t: &str| -> Result<i64> {
        Ok(c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?)
    };
    Ok(Manifest {
        file: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        created_at: db::now_ms(),
        bytes: path.metadata()?.len(),
        sha256: sha256(path)?,
        schema_version,
        sessions: count("sessions")?,
        events: count("events")?,
        memories: count("memories")?,
    })
}

/// Snapshot `conn`'s database into `dir`, verify it, write its manifest, rotate old ones.
pub fn create(conn: &Connection, dir: &Path, keep: usize) -> Result<Manifest> {
    std::fs::create_dir_all(dir)?;
    // One snapshot at a time: the watcher and a manual `mnem backup` can start together.
    let _lock = Lock::acquire(dir)?;
    let stamp = chrono_stamp(db::now_ms());
    let mut path = dir.join(format!("mnem-{stamp}.db"));
    if path.exists() {
        path = dir.join(format!("mnem-{stamp}-{}.db", std::process::id()));
    }
    let tmp = dir.join(format!(".mnem-{stamp}-{}.db.partial", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])?;
    if let Err(e) = stamp_origin(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context("could not record the snapshot's origin"));
    }
    // Only a verified snapshot gets its final name.
    let m = match inspect(&tmp) {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.context("snapshot failed verification"));
        }
    };
    std::fs::rename(&tmp, &path)?;
    let m = Manifest {
        file: path.file_name().unwrap().to_string_lossy().into_owned(),
        ..m
    };
    std::fs::write(manifest_path(&path), serde_json::to_string_pretty(&m)?)?;
    rotate(dir, keep)?;
    Ok(m)
}

/// Record in a fresh snapshot where and when it was taken and the settings in use.
fn stamp_origin(snapshot: &Path) -> Result<()> {
    let c = Connection::open(snapshot)?;
    let config = std::fs::read_to_string(crate::config::path()).ok();
    let mut put = c.prepare(
        "INSERT INTO meta(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
    )?;
    put.execute(params!["export.host", hostname()])?;
    put.execute(params!["export.at", db::now_ms().to_string()])?;
    put.execute(params!["export.version", env!("CARGO_PKG_VERSION")])?;
    match config {
        Some(cfg) => put.execute(params!["export.config", cfg])?,
        None => c.execute("DELETE FROM meta WHERE k = 'export.config'", [])?,
    };
    Ok(())
}

/// This machine's name, for telling backups from different machines apart.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Where a snapshot came from, as recorded when it was taken (older snapshots have none).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Origin {
    pub host: Option<String>,
    pub taken_at: Option<i64>,
    pub version: Option<String>,
    /// The settings file in use on that machine, if any.
    pub config: Option<String>,
}

pub fn origin(snapshot: &Path) -> Result<Origin> {
    let c = Connection::open_with_flags(snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let get = |k: &str| -> Option<String> {
        c.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .ok()
    };
    Ok(Origin {
        host: get("export.host"),
        taken_at: get("export.at").and_then(|v| v.parse().ok()),
        version: get("export.version"),
        config: get("export.config"),
    })
}

/// Check a database file from elsewhere before it can be restored: it must be a sound
/// mnem database this build can read. Returns its manifest and origin.
pub fn check_import(path: &Path) -> Result<(Manifest, Origin)> {
    let mut head = [0u8; 16];
    std::io::Read::read_exact(&mut std::fs::File::open(path)?, &mut head)
        .context("file is too short to be a database")?;
    ensure!(&head == b"SQLite format 3\0", "not a SQLite database");
    let schema: i64 = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
        .query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        schema <= db::SCHEMA_VERSION,
        "this backup comes from a newer mnem (schema {schema}, this build reads up to {}); update mnem first",
        db::SCHEMA_VERSION
    );
    let m = verify(path)?;
    ensure!(
        m.sessions + m.events + m.memories > 0,
        "the backup holds no sessions, events or memories"
    );
    Ok((m, origin(path)?))
}

/// What adopting a snapshot's settings would change on this machine.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsReview {
    /// The settings parse as a mnem config.
    pub valid: bool,
    pub error: Option<String>,
    /// Setting paths (e.g. distill.base_url) whose value differs: (path, here, backup).
    pub changes: Vec<(String, Value, Value)>,
    /// Changes that decide where prompts and memories are sent, or which key is used.
    pub sensitive: Vec<String>,
    /// Why this build could not use them fully, if so.
    pub warning: Option<String>,
}

fn flatten(prefix: &str, v: &Value, out: &mut std::collections::BTreeMap<String, Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&p, x, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), v.clone());
        }
    }
}

pub fn review_settings(snapshot: &Path) -> Result<Option<SettingsReview>> {
    let Some(cfg) = origin(snapshot)?.config else {
        return Ok(None);
    };
    let parsed = serde_json::from_str::<crate::config::Config>(&cfg);
    let theirs: Value = serde_json::from_str(&cfg).unwrap_or(Value::Null);
    let ours: Value = std::fs::read_to_string(crate::config::path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| Value::Object(Default::default()));
    let (mut a, mut b) = (Default::default(), Default::default());
    flatten("", &ours, &mut a);
    flatten("", &theirs, &mut b);
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let changes: Vec<(String, Value, Value)> = keys
        .into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| {
            (
                k.clone(),
                a.get(k).cloned().unwrap_or(Value::Null),
                b.get(k).cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    let sensitive = changes
        .iter()
        .map(|c| c.0.clone())
        .filter(|k| k.starts_with("distill.base_url") || k.starts_with("distill.api_key"))
        .collect();
    let model = b
        .get("semantic.model")
        .and_then(Value::as_str)
        .unwrap_or("");
    let warning = if model.starts_with("fastembed:") && !cfg!(feature = "fastembed") {
        Some(format!(
            "this mnem was built without ONNX support, so it cannot run {model}; recall would use keywords only. Install with `cargo install --features fastembed` first, or keep this machine's settings."
        ))
    } else if !model.is_empty() && model != crate::embed::model_name() {
        Some(format!(
            "{model} is not this machine's embedding model: after the restart mnem downloads it if needed (internet required) and re-embeds every memory in the background, which can take a while; recall uses keywords until then."
        ))
    } else {
        None
    };
    Ok(Some(SettingsReview {
        valid: parsed.is_ok(),
        error: parsed.err().map(|e| e.to_string()),
        changes,
        sensitive,
        warning,
    }))
}

/// Transcripts the database knows but this machine does not have: after a move, mark
/// them as another machine's (their history is kept) instead of "deleted by agent".
/// A file that shows up later is read again as usual.
pub fn mark_foreign_sources(conn: &Connection) -> Result<usize> {
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM sources WHERE excluded = 0")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    let mut st = conn.prepare("UPDATE sources SET excluded = 2 WHERE path = ?1")?;
    for p in paths.iter().filter(|p| !Path::new(p).exists()) {
        n += st.execute([p])?;
    }
    Ok(n)
}

/// Free bytes on the filesystem holding `path`, when the platform can tell.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 {
            return Some(s.f_bavail as u64 * s.f_frsize as u64);
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Replace config.json with the settings a snapshot carried, keeping the current file
/// as config.json.bak-<time>. Returns false when the snapshot carried none. The
/// settings must parse as a mnem config; they take effect when mnem restarts.
pub fn apply_settings(snapshot: &Path) -> Result<bool> {
    let Some(cfg) = origin(snapshot)?.config else {
        return Ok(false);
    };
    apply_settings_file(&cfg)?;
    Ok(true)
}

/// Write settings text as config.json (validated first), keeping the old file.
pub fn apply_settings_file(cfg: &str) -> Result<()> {
    serde_json::from_str::<crate::config::Config>(cfg)
        .context("the backup's settings are not a valid mnem config")?;
    let path = crate::config::path();
    if path.exists() {
        std::fs::copy(
            &path,
            db::data_dir().join(format!("config.json.bak-{}", chrono_stamp(db::now_ms()))),
        )?;
    }
    let tmp = db::data_dir().join("config.json.partial");
    std::fs::write(&tmp, cfg)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Snapshots in `dir`, newest first, with their manifests when present.
pub fn list(dir: &Path) -> Result<Vec<(PathBuf, Option<Manifest>)>> {
    let mut out: Vec<(PathBuf, Option<Manifest>)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                n.starts_with("mnem-") && n.ends_with(".db")
            })
            .map(|p| {
                let m = std::fs::read_to_string(manifest_path(&p))
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok());
                (p, m)
            })
            .collect(),
        Err(_) => vec![],
    };
    // Newest first by modification time; names mix manual and automatic stamps.
    let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
    out.sort_by_key(|(p, _)| std::cmp::Reverse(mtime(p)));
    Ok(out)
}

fn rotate(dir: &Path, keep: usize) -> Result<()> {
    for (path, _) in list(dir)?.into_iter().skip(keep.max(1)) {
        let _ = std::fs::remove_file(manifest_path(&path));
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Whether `mnem watch` takes a snapshot by itself every day (on unless switched off in
/// the viewer or with `mnem backup --auto off`). Kept in the database, so the running
/// watcher follows a change on its next pass.
pub fn auto_enabled(conn: &Connection) -> bool {
    conn.query_row("SELECT v FROM meta WHERE k = 'backup.auto'", [], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .is_none_or(|v| v != "off")
}

pub fn set_auto(conn: &Connection, on: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(k, v) VALUES ('backup.auto', ?1) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        [if on { "on" } else { "off" }],
    )?;
    Ok(())
}

/// Delete one snapshot and its manifest. `file` must be a plain snapshot name in `dir`
/// (as `list` returns it): nothing outside the folder, nothing else in it.
pub fn remove(dir: &Path, file: &str) -> Result<()> {
    ensure!(
        file.starts_with("mnem-")
            && file.ends_with(".db")
            && !file.contains('/')
            && !file.contains('\\')
            && !file.contains(".."),
        "not a backup name: {file}"
    );
    let path = dir.join(file);
    ensure!(path.is_file(), "no such backup: {file}");
    let _ = std::fs::remove_file(manifest_path(&path));
    std::fs::remove_file(&path)?;
    Ok(())
}

/// Age in ms of the newest verified snapshot, or None when there is none.
pub fn newest_age(dir: &Path) -> Option<i64> {
    list(dir)
        .ok()?
        .into_iter()
        .find_map(|(_, m)| m)
        .map(|m| db::now_ms() - m.created_at)
}

/// Prove a snapshot restores: copy it to a scratch file, check integrity and counts
/// against its manifest, run migrations, check the full-text indexes and search for a
/// word that is known to be in them. Returns the verified manifest.
pub fn verify(snapshot: &Path) -> Result<Manifest> {
    let (m, staged) = stage(snapshot)?;
    remove_db(&staged);
    Ok(m)
}

/// Verified copy of `snapshot` outside the backups directory (so rotation can never
/// delete it), plus its manifest. The caller removes the copy.
fn stage(snapshot: &Path) -> Result<(Manifest, PathBuf)> {
    let want: Option<Manifest> = std::fs::read_to_string(manifest_path(snapshot))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    // Beside the snapshot (same disk, counted by the import space check), with a name
    // rotation never matches.
    let staged = snapshot
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".restore-{}-{}.db",
            std::process::id(),
            db::now_ms()
        ));
    std::fs::copy(snapshot, &staged).with_context(|| format!("copy {}", snapshot.display()))?;
    let result = (|| -> Result<Manifest> {
        let got = inspect(&staged)?;
        if let Some(w) = &want {
            ensure!(got.sha256 == w.sha256, "checksum differs from manifest");
            ensure!(
                (got.sessions, got.events, got.memories) == (w.sessions, w.events, w.memories),
                "row counts differ from manifest"
            );
        }
        // Its own triggers, views and indexes go before anything writes to it (a trigger
        // smuggled in the file would run on mnem's schema setup and migrations); then it
        // is opened through mnem, which migrates it exactly as a restored database.
        {
            let raw = rusqlite::Connection::open(&staged)?;
            db::sanitize_schema(&raw)?;
        }
        let c = db::open(&staged)?;
        for t in ["memories_fts", "events_fts"] {
            c.execute(
                &format!("INSERT INTO {t}({t}) VALUES ('integrity-check')"),
                [],
            )
            .with_context(|| format!("{t} index is inconsistent"))?;
        }
        // Search for a word taken from a stored title, not an assumed English word.
        let word: Option<String> = c
            .query_row(
                "SELECT title FROM memories WHERE length(coalesce(title, '')) > 3 LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|t| {
                t.split(|ch: char| !ch.is_alphanumeric())
                    .find(|w| w.len() >= 3)
                    .map(str::to_lowercase)
            });
        if let Some(w) = word {
            let hits: i64 = c.query_row(
                "SELECT count(*) FROM (SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1 LIMIT 1)",
                [format!("\"{w}\"")],
                |r| r.get(0),
            )?;
            ensure!(hits > 0, "full-text search found nothing for a stored word");
        }
        Ok(got)
    })();
    match result {
        Ok(got) => {
            let file = snapshot
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok((Manifest { file, ..got }, staged))
        }
        Err(e) => {
            remove_db(&staged);
            Err(e)
        }
    }
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Restore a verified snapshot into the live database with SQLite's online-backup API.
/// The copy happens inside SQLite's own locking, so other connections (hooks, the
/// watcher, MCP servers) wait and then see the restored data; no file is swapped or
/// unlinked underneath them. The current database is snapshotted first.
pub fn restore(snapshot: &Path, conn: &mut Connection, backups: &Path) -> Result<Manifest> {
    restore_with_wait(snapshot, conn, backups, RESTORE_WAIT)
}

fn restore_with_wait(
    snapshot: &Path,
    conn: &mut Connection,
    backups: &Path,
    wait: std::time::Duration,
) -> Result<Manifest> {
    // Stage first: the pre-restore snapshot below rotates old backups, which could
    // otherwise delete the very snapshot being restored.
    let (m, staged) = stage(snapshot)?;
    let result = (|| -> Result<()> {
        let live = conn
            .path()
            .map(PathBuf::from)
            .context("the live database has no file path")?;
        let src = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let backup = rusqlite::backup::Backup::new(&src, conn)?;
        // First take the live database's write lock (a step of zero pages) and hold it
        // until the copy is done, so nothing can commit between the safety snapshot and
        // the replacement. Writers wait; hooks fail open and catch up from transcripts.
        let deadline = std::time::Instant::now() + wait;
        loop {
            use rusqlite::backup::StepResult::{Busy, Done, Locked, More};
            match backup.step(0)? {
                More | Done => break,
                Busy | Locked if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(100))
                }
                Busy | Locked => bail!(
                    "another process kept the database locked for {}s; nothing was changed. Retry, or stop mnem-watch.service first",
                    wait.as_secs()
                ),
                _ => bail!("unexpected backup step result"),
            }
        }
        // The snapshot reads through its own connection; readers are not blocked.
        let reader = Connection::open_with_flags(&live, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let keep = create(&reader, backups, KEEP + 1).context(
            "could not snapshot the current database before restoring; nothing was changed",
        )?;
        println!("current database saved as {}", keep.file);
        loop {
            use rusqlite::backup::StepResult::{Busy, Done, Locked, More};
            match backup.step(i32::MAX)? {
                Done => break,
                More => continue,
                Busy | Locked => {
                    bail!("lost the database lock during restore; nothing was changed")
                }
                _ => bail!("unexpected backup step result"),
            }
        }
        Ok(())
    })();
    remove_db(&staged);
    result?;
    Ok(m)
}

/// Exclusive backup lock (a file created with create_new); stale after 30 minutes.
struct Lock(PathBuf);

impl Lock {
    fn acquire(dir: &Path) -> Result<Lock> {
        let path = dir.join(".backup.lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Lock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = path
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(1800));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                    } else if std::time::Instant::now() > deadline {
                        bail!("another backup is still running");
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// yyyymmdd-hhmmss (UTC) without a date library.
fn chrono_stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_is_removed_one_at_a_time_and_only_backups() {
        let d = std::env::temp_dir().join(format!("mnem-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let c = crate::db::open(&d.join("m.db")).unwrap();
        let backups = d.join("backups");
        let a = create(&c, &backups, KEEP).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = create(&c, &backups, KEEP).unwrap();
        assert_ne!(a.file, b.file);
        remove(&backups, &a.file).unwrap();
        let left: Vec<String> = list(&backups)
            .unwrap()
            .into_iter()
            .filter_map(|(_, m)| m)
            .map(|m| m.file)
            .collect();
        assert_eq!(
            left,
            std::slice::from_ref(&b.file),
            "only the chosen one went"
        );
        assert!(
            !backups.join(a.file.replace(".db", ".json")).exists(),
            "its manifest too"
        );
        // Anything that is not a backup in this folder is refused.
        std::fs::write(d.join("mnem-outside.db"), "x").unwrap();
        for bad in [
            "../mnem-outside.db",
            "m.db",
            "mnem-x.json",
            "/etc/passwd",
            "mnem-..db",
            &a.file,
        ] {
            assert!(remove(&backups, bad).is_err(), "{bad}");
        }
        assert!(d.join("mnem-outside.db").exists() && d.join("m.db").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_daily_backup_switch_is_on_by_default_and_remembered() {
        let c =
            crate::db::open_with(Path::new(":memory:"), std::time::Duration::from_secs(1)).unwrap();
        assert!(auto_enabled(&c), "on until switched off");
        set_auto(&c, false).unwrap();
        assert!(!auto_enabled(&c));
        // Off, a missing or old backup is not a health alert (backing up is the user's).
        assert!(
            !crate::health::for_hook(&c)
                .iter()
                .any(|a| a.contains("backup")),
            "{:?}",
            crate::health::for_hook(&c)
        );
        set_auto(&c, true).unwrap();
        assert!(auto_enabled(&c));
    }

    #[test]
    fn concurrent_backups_do_not_collide() {
        let d = std::env::temp_dir().join(format!("mnem-backup-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("m.db");
        drop(db::open(&path).unwrap());
        let backups = d.join("backups");
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (path, backups) = (path.clone(), backups.clone());
                std::thread::spawn(move || {
                    let c = db::open(&path).unwrap();
                    create(&c, &backups, 7).map(|m| m.file)
                })
            })
            .collect();
        let files: Vec<String> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        assert_ne!(files[0], files[1]);
        assert_eq!(list(&backups).unwrap().len(), 2);
    }

    #[test]
    fn restore_gives_up_cleanly_when_locked() {
        let d = std::env::temp_dir().join(format!("mnem-restore-locked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let backups = d.join("backups");
        let mut conn = db::open(&d.join("m.db")).unwrap();
        conn.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'snap', 'mnem', 'a')", [])
            .unwrap();
        let snap = create(&conn, &backups, 7).unwrap();
        conn.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'live', 'mnem', 'b')", [])
            .unwrap();
        // Another writer holds the write lock for the whole attempt.
        let holder = db::open(&d.join("m.db")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.busy_timeout(std::time::Duration::from_millis(10))
            .unwrap();
        let t = std::time::Instant::now();
        let r = restore_with_wait(
            &backups.join(&snap.file),
            &mut conn,
            &backups,
            std::time::Duration::from_millis(500),
        );
        assert!(r.is_err(), "restore must fail while locked");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "and must not hang"
        );
        holder.execute_batch("ROLLBACK").unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "live database unchanged");
    }

    /// A trigger in the backup never runs: not on the writes mnem's schema setup makes
    /// while the backup is checked, and not by a name that looks like SQLite's own.
    #[test]
    fn a_smuggled_trigger_never_runs_during_restore() {
        let d = std::env::temp_dir().join(format!("mnem-restore-early-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let evil = d.join("evil.db");
        {
            let c = db::open(&evil).unwrap();
            c.execute(
                "INSERT INTO sessions(id, agent, native_id, title) VALUES ('pi:s', 'pi', 's', 'clean')",
                [],
            )
            .unwrap();
            c.execute_batch(
                "CREATE TRIGGER early_meta BEFORE INSERT ON meta BEGIN UPDATE sessions SET title = 'pwned'; END;
                 CREATE TRIGGER sqliteevil BEFORE INSERT ON meta BEGIN UPDATE sessions SET title = 'pwned'; END;
                 CREATE TRIGGER sqliteXevil AFTER UPDATE ON sessions BEGIN UPDATE sessions SET native_id = 'pwned' WHERE native_id != 'pwned'; END;
                 DELETE FROM meta;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let mut conn = db::open(&d.join("m.db")).unwrap();
        restore(&evil, &mut conn, &d.join("backups")).unwrap();
        let title: String = conn
            .query_row("SELECT title FROM sessions WHERE id = 'pi:s'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "clean");
        let left: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND (name GLOB '*evil*' OR name GLOB 'early*')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(left.is_empty(), "{left:?}");
        conn.execute(
            "UPDATE sessions SET title = 'renamed' WHERE id = 'pi:s'",
            [],
        )
        .unwrap();
        let native: String = conn
            .query_row(
                "SELECT native_id FROM sessions WHERE id = 'pi:s'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(native, "s");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn restore_drops_code_a_backup_smuggles_in() {
        let d = std::env::temp_dir().join(format!("mnem-restore-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let backups = d.join("backups");
        let evil = d.join("evil.db");
        {
            let c = db::open(&evil).unwrap();
            c.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'kept', 'mnem', 'a')", [])
                .unwrap();
            c.execute_batch(
                "CREATE TRIGGER wipe AFTER INSERT ON memories BEGIN DELETE FROM memories; END;
                 CREATE VIEW peek AS SELECT * FROM memories;
                 CREATE TABLE stash(x);
                 DROP TRIGGER memories_vec_ad;
                 CREATE TRIGGER memories_vec_ad AFTER DELETE ON memories BEGIN DELETE FROM sessions; END;",
            )
            .unwrap();
        }
        let mut conn = db::open(&d.join("m.db")).unwrap();
        restore(&evil, &mut conn, &backups).unwrap();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE name IN ('wipe', 'peek', 'stash')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(names.is_empty(), "{names:?}");
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'memories_vec_ad'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("memory_vectors") && !sql.contains("sessions"),
            "{sql}"
        );
        conn.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'new', 'mnem', 'b')", [])
            .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn no_write_is_lost_during_restore() {
        let d = std::env::temp_dir().join(format!("mnem-restore-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let backups = d.join("backups");
        let live = d.join("m.db");
        let mut conn = db::open(&live).unwrap();
        let src = d.join("other.db");
        {
            let c = db::open(&src).unwrap();
            c.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'imported', 'mnem', 'x')", [])
                .unwrap();
        }
        // A writer keeps committing while the restore runs.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (live, stop) = (live.clone(), stop.clone());
            std::thread::spawn(move || {
                let c = db::open_with(&live, std::time::Duration::from_millis(5)).unwrap();
                let mut ok = Vec::new();
                let mut i = 0;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    i += 1;
                    let key = format!("w{i}");
                    if c.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'w', 'mnem', ?1)", [&key]).is_ok() {
                        ok.push(key);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                ok
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        restore(&src, &mut conn, &backups).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let committed = writer.join().unwrap();
        assert!(!committed.is_empty());
        // Every committed write is in the restored database or in the safety snapshot.
        let snap = list(&backups).unwrap()[0].0.clone();
        let s = Connection::open(&snap).unwrap();
        for key in committed {
            let q = "SELECT count(*) FROM memories WHERE origin_id = ?1";
            let here: i64 = conn.query_row(q, [&key], |r| r.get(0)).unwrap();
            let saved: i64 = s.query_row(q, [&key], |r| r.get(0)).unwrap();
            assert!(here + saved > 0, "write {key} was lost");
        }
    }

    #[test]
    fn stamps() {
        assert_eq!(chrono_stamp(0), "19700101-000000");
        assert_eq!(chrono_stamp(1_790_507_604_101), "20260927-111324");
    }

    #[test]
    fn backup_verify_rotate() {
        let d = std::env::temp_dir().join(format!("mnem-backup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = db::open(&d.join("m.db")).unwrap();
        conn.execute(
            "INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'the queue stalled', 'mnem', 'x')",
            [],
        )
        .unwrap();
        let backups = d.join("backups");
        let mut made = Vec::new();
        for _ in 0..3 {
            made.push(create(&conn, &backups, 2).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        let listed = list(&backups).unwrap();
        assert_eq!(listed.len(), 2, "rotation keeps 2");
        assert_eq!(listed[0].1.as_ref().unwrap().memories, 1);
        let v = verify(&listed[0].0).unwrap();
        assert_eq!(v.memories, 1);
        // Restore the older snapshot over a live database that has moved on.
        conn.execute(
            "INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'later', 'mnem', 'y')",
            [],
        )
        .unwrap();
        // A second connection stays open across the restore, as MCP servers would.
        let reader = db::open(&d.join("m.db")).unwrap();
        let mut conn = conn;
        let older = listed[1].0.clone();
        restore(&older, &mut conn, &backups).unwrap();
        let n: i64 = reader
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n, 1,
            "open connections see the restored state, not the later write"
        );
        // Restoring must not have deleted its own source through rotation.
        assert!(older.exists() || list(&backups).unwrap().len() <= 3);
        let listed = list(&backups).unwrap();
        // Corrupt the newest snapshot: verification must refuse it.
        std::fs::write(&listed[0].0, b"not a database").unwrap();
        assert!(verify(&listed[0].0).is_err());
    }

    #[test]
    fn restoring_the_oldest_snapshot_survives_rotation() {
        let d = std::env::temp_dir().join(format!("mnem-restore-oldest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let backups = d.join("backups");
        let mut conn = db::open(&d.join("m.db")).unwrap();
        conn.execute("INSERT INTO memories(kind, title, origin, origin_id) VALUES ('observation', 'first', 'mnem', 'a')", [])
            .unwrap();
        // Fill the rotation window so the pre-restore snapshot pushes the oldest out.
        let mut made = Vec::new();
        for _ in 0..(KEEP + 1) {
            made.push(create(&conn, &backups, KEEP + 1).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(1050));
        }
        let oldest = backups.join(&made[0].file);
        restore(&oldest, &mut conn, &backups).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
