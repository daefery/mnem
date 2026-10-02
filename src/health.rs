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

/// Full check, including the costly parts (a stat per transcript, a systemctl call).
/// For `mnem doctor`, the viewer and the watcher; hooks use `for_hook`.
pub fn alerts(conn: &Connection, stuck_files: usize) -> Vec<String> {
    let mut out = Vec::new();
    if stuck_files > 0 {
        out.push(stuck_message(stuck_files));
    }
    out.extend(cheap(conn));
    if watch_installed() && !watch_active() {
        out.push(
            "mnem-watch.service is not running (systemctl --user start mnem-watch.service)".into(),
        );
    }
    out
}

fn stuck_message(n: usize) -> String {
    format!(
        "capture is stuck: {n} transcript(s) changed over 5 minutes ago and are still not indexed (run `mnem doctor`)"
    )
}

/// The watcher records the costly checks here every pass.
pub fn record_watch_report(conn: &Connection, stuck_files: usize) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO meta(k, v) VALUES ('health.watch', ?1) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        [serde_json::json!({ "at": db::now_ms(), "stuck": stuck_files }).to_string()],
    )?;
    Ok(())
}

/// Hot-path check for hooks: SQL and a directory listing only. The costly checks come
/// from the watcher's last report; a stale report means the watcher is not running.
pub fn for_hook(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    let report: Option<serde_json::Value> = conn
        .query_row("SELECT v FROM meta WHERE k = 'health.watch'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    if watch_installed() {
        match report {
            Some(r) => {
                let age = db::now_ms() - r["at"].as_i64().unwrap_or(0);
                if age > 5 * 60_000 {
                    out.push(format!(
                        "mnem-watch has not reported for {} (is it running? systemctl --user status mnem-watch.service)",
                        ago(age)
                    ));
                } else if let Some(n) = r["stuck"].as_u64().filter(|n| *n > 0) {
                    out.push(stuck_message(n as usize));
                }
            }
            None => out.push(
                "mnem-watch has never reported (systemctl --user status mnem-watch.service)".into(),
            ),
        }
    }
    out.extend(cheap(conn));
    out
}

/// Checks that cost only SQL and a directory listing.
fn cheap(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    let now = db::now_ms();

    // Backups (only when mnem takes them itself: switched off, backing up is the user's).
    match backup::newest_age(&backup::dir()) {
        _ if !backup::auto_enabled(conn) => {}
        None => out.push("no verified backup yet (run `mnem backup`)".into()),
        Some(age) if age > 2 * backup::INTERVAL_MS => out.push(format!(
            "last verified backup is {} old (run `mnem backup`)",
            ago(age)
        )),
        _ => {}
    }

    // Distillation that never runs: capture and search work, but no memory is made, so
    // recall, file memories and summaries stay empty with nothing else saying why.
    if let Some(why) = distill::not_configured(&CONFIG.distill) {
        out.push(why.into());
    }

    // Distillation: sessions about to leave the backfill window undistilled are lost to
    // recall (their transcript stays, but no memory is ever made from it).
    if let Ok(b) = distill::backlog(conn) {
        if b.falling_behind() {
            out.push(format!(
                "{} session(s) will leave the distillation window undistilled within a day (backfill is not keeping up; see mnem doctor): run `mnem distill --oldest-first --since-days {} --limit 1000` or raise distill.daily_calls",
                b.at_risk, b.days
            ));
        }
        if b.expired > 0 {
            out.push(format!(
                "{} session(s) aged out of distillation undistilled: no memories were made from them (run `mnem distill --aged-out --limit {}`)",
                b.expired, b.expired
            ));
        }
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
    let cli = CONFIG
        .distill
        .provider
        .as_deref()
        .and_then(crate::cli_llm::Cli::from_name);
    let chain: Vec<String> = match (&CONFIG.distill.models, cli) {
        (Some(m), _) => m.clone(),
        (None, Some(c)) => c.default_chain().iter().map(|s| s.to_string()).collect(),
        (None, None) => distill::DEFAULT_CHAIN
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let configured = distill::not_configured(&CONFIG.distill).is_none();
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
