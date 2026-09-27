//! Health checks shared by `mnem doctor`, session-start hooks (shown to the user) and
//! the viewer. The original failure this tool replaces was silent: memory stopped
//! being saved and nobody noticed. Every check here exists so that cannot happen.

use crate::backup;
use crate::config::CONFIG;
use crate::context::ago;
use crate::db;
use crate::distill;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;

/// Capture that is behind for longer than this is an alert, not normal lag.
const STUCK_MS: i64 = 5 * 60_000;

/// Problems worth interrupting the user for. Empty when all is well.
pub fn alerts(conn: &Connection, stuck_files: usize) -> Vec<String> {
    let mut out = Vec::new();
    let now = db::now_ms();

    if stuck_files > 0 {
        out.push(format!(
            "capture is stuck: {stuck_files} transcript(s) changed over 5 minutes ago and are still not indexed (run `mnem doctor`)"
        ));
    }

    // Backups.
    match backup::newest_age(&backup::dir()) {
        None => out.push("no verified backup yet (run `mnem backup`)".into()),
        Some(age) if age > 2 * backup::INTERVAL_MS => out.push(format!(
            "last verified backup is {} old (run `mnem backup`)",
            ago(age)
        )),
        _ => {}
    }

    // Distillation: every configured model cooling down means summaries have stopped.
    let cooling: HashMap<String, i64> = conn
        .query_row(
            "SELECT v FROM meta WHERE k = 'distill.cooldowns'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let chain: Vec<String> = match &CONFIG.distill.models {
        Some(m) => m.clone(),
        None => distill::DEFAULT_CHAIN
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let configured = CONFIG.distill.api_key_env.is_some() || CONFIG.distill.api_key_json.is_some();
    if configured
        && !chain.is_empty()
        && chain
            .iter()
            .all(|m| cooling.get(m).is_some_and(|u| *u > now))
    {
        let until = chain
            .iter()
            .filter_map(|m| cooling.get(m))
            .min()
            .copied()
            .unwrap_or(now);
        out.push(format!(
            "summaries paused: every configured model is cooling down (next retry in {})",
            ago(until - now)
        ));
    }
    let recent_error: Option<String> = conn
        .query_row(
            "SELECT error FROM distill_state WHERE error IS NOT NULL AND updated_at > ?1 ORDER BY updated_at DESC LIMIT 1",
            [now - 6 * 3_600_000],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if let Some(e) = recent_error {
        out.push(format!("summaries failing: {}", crate::text::head(&e, 140)));
    }

    // The watcher does reconciliation, nightly backups and the viewer.
    if watch_installed() && !watch_active() {
        out.push(
            "mnem-watch.service is not running (systemctl --user start mnem-watch.service)".into(),
        );
    }
    out
}

/// Files whose unread bytes are older than STUCK_MS. Costs one stat per tracked file.
pub fn stuck_files(conn: &Connection) -> usize {
    let now = db::now_ms();
    let Ok(mut st) = conn.prepare(
        "SELECT path, byte_offset FROM sources WHERE excluded = 0 AND missing_since IS NULL",
    ) else {
        return 0;
    };
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)));
    let Ok(rows) = rows else { return 0 };
    rows.filter_map(Result::ok)
        .filter(|(p, off)| {
            std::path::Path::new(p).metadata().is_ok_and(|m| {
                let age = crate::ingest::mtime_ms(&m).map(|t| now - t).unwrap_or(0);
                m.len() > *off as u64 && age > STUCK_MS && has_complete_line(p, *off as u64)
            })
        })
        .count()
}

/// A half-written last line (e.g. from a crashed session) is not a stuck capture:
/// ingest deliberately waits for the newline. Only complete unread lines count.
fn has_complete_line(path: &str, offset: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut buf = [0u8; 64 * 1024];
    let mut reader = f.take(64 << 20);
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) if buf[..n].contains(&b'\n') => return true,
            Ok(_) => {}
        }
    }
}

fn watch_installed() -> bool {
    db::home()
        .join(".config/systemd/user/mnem-watch.service")
        .exists()
}

fn watch_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "mnem-watch.service"])
        .status()
        .is_ok_and(|s| s.success())
}
