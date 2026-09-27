use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT);

-- One row per transcript file. The cursor (offset + parser_state) is committed in the
-- same transaction as the events it produced.
CREATE TABLE IF NOT EXISTS sources(
  path TEXT PRIMARY KEY,
  agent TEXT NOT NULL,
  fingerprint TEXT,
  generation INTEGER NOT NULL DEFAULT 0,
  byte_offset INTEGER NOT NULL DEFAULT 0,
  size_seen INTEGER NOT NULL DEFAULT 0,
  parser_state TEXT,
  session_id TEXT,
  excluded INTEGER NOT NULL DEFAULT 0,
  bad_lines INTEGER NOT NULL DEFAULT 0,
  last_ingest_at INTEGER,
  missing_since INTEGER
);

CREATE TABLE IF NOT EXISTS sessions(
  id TEXT PRIMARY KEY,               -- '<agent>:<native id>'
  agent TEXT NOT NULL,
  native_id TEXT NOT NULL,
  project TEXT,
  cwd TEXT,
  git_branch TEXT,
  title TEXT,
  started_at INTEGER,
  last_event_at INTEGER
);
CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project, last_event_at);

CREATE TABLE IF NOT EXISTS events(
  id INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  record_key TEXT NOT NULL,
  ts INTEGER,
  turn INTEGER,
  kind TEXT NOT NULL,
  tool TEXT,
  path TEXT,
  text TEXT,
  is_error INTEGER NOT NULL DEFAULT 0,
  source_path TEXT,
  byte_offset INTEGER,
  UNIQUE(session_id, record_key)
);
CREATE INDEX IF NOT EXISTS events_session ON events(session_id, id);
CREATE INDEX IF NOT EXISTS events_path ON events(path) WHERE path IS NOT NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
  text, path, content='events', content_rowid='id', tokenize='porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events BEGIN
  INSERT INTO events_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
  INSERT INTO events_fts(events_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
END;

-- Lines that failed to parse. Kept for replay once an adapter learns the format.
CREATE TABLE IF NOT EXISTS quarantine(
  source_path TEXT NOT NULL,
  byte_offset INTEGER NOT NULL,
  reason TEXT,
  line TEXT,
  PRIMARY KEY(source_path, byte_offset)
);
"#;

pub fn data_dir() -> PathBuf {
    std::env::var_os("MNEM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".mnem"))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
