//! Backups: consistent snapshots of the database, verified before they are trusted.
//!
//! A snapshot is written with `VACUUM INTO` (consistent even while hooks and the
//! watcher write), then reopened and checked: `integrity_check`, schema version, row
//! counts and a SHA-256, recorded in a `.json` manifest beside it. A backup that was
//! never verified is not counted as a backup.

use crate::db;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const KEEP: usize = 7;
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

/// Age in ms of the newest verified snapshot, or None when there is none.
pub fn newest_age(dir: &Path) -> Option<i64> {
    list(dir)
        .ok()?
        .into_iter()
        .find_map(|(_, m)| m)
        .map(|m| db::now_ms() - m.created_at)
}

/// Prove a snapshot restores: copy it to a scratch file, check integrity and counts
/// against its manifest, and run a real search on it. Returns the verified manifest.
pub fn verify(snapshot: &Path) -> Result<Manifest> {
    let want: Option<Manifest> = std::fs::read_to_string(manifest_path(snapshot))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let scratch =
        std::env::temp_dir().join(format!("mnem-restore-check-{}.db", std::process::id()));
    std::fs::copy(snapshot, &scratch)?;
    let result = (|| -> Result<Manifest> {
        let got = inspect(&scratch)?;
        if let Some(w) = &want {
            ensure!(got.sha256 == w.sha256, "checksum differs from manifest");
            ensure!(
                (got.sessions, got.events, got.memories) == (w.sessions, w.events, w.memories),
                "row counts differ from manifest"
            );
        }
        // Opening through mnem runs migrations exactly as a restored database would.
        let c = db::open(&scratch)?;
        let hits: i64 = c.query_row(
            "SELECT count(*) FROM (SELECT rowid FROM memories_fts WHERE memories_fts MATCH 'the' LIMIT 5)",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            got.memories == 0 || hits > 0,
            "full-text index returned nothing"
        );
        Ok(got)
    })();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", scratch.display()));
    }
    let file = snapshot
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    result.map(|m| Manifest { file, ..m })
}

/// Replace the live database with a verified snapshot. The current database is kept as
/// a pre-restore snapshot first. Refuses while another mnem process may be writing.
pub fn restore(snapshot: &Path, live: &Path, conn: Connection) -> Result<Manifest> {
    let m = verify(snapshot)?;
    if watch_is_running() {
        bail!(
            "mnem-watch.service is running; stop it first: systemctl --user stop mnem-watch.service"
        );
    }
    let keep = create(&conn, &dir(), KEEP + 1)
        .context("could not snapshot the current database before restoring")?;
    println!("current database saved as {}", keep.file);
    drop(conn);
    swap_in(snapshot, live)?;
    Ok(m)
}

/// Copy the snapshot next to the live file, drop the live WAL, then rename over it,
/// so the live path always holds either the old database or the complete new one.
fn swap_in(snapshot: &Path, live: &Path) -> Result<()> {
    let staged = live.with_extension("db.restoring");
    std::fs::copy(snapshot, &staged)?;
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", live.display()));
    }
    std::fs::rename(&staged, live)?;
    Ok(())
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

fn watch_is_running() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "mnem-watch.service"])
        .status()
        .is_ok_and(|s| s.success())
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
        drop(conn);
        swap_in(&listed[1].0, &d.join("m.db")).unwrap();
        let back = db::open(&d.join("m.db")).unwrap();
        let n: i64 = back
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "restored state, not the later write");
        // Corrupt the newest snapshot: verification must refuse it.
        std::fs::write(&listed[0].0, b"not a database").unwrap();
        assert!(verify(&listed[0].0).is_err());
    }
}
