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
  mtime_seen INTEGER,                -- mtime (ms) when last ingested; with size, gates re-reads
  parser_state TEXT,
  session_id TEXT,
  checkpoint TEXT,                   -- hash of the bytes just before byte_offset
  file_id TEXT,                      -- dev:inode, detects replace-by-rename
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
  thread TEXT,
  label TEXT,
  tool_raw TEXT,
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
CREATE TRIGGER IF NOT EXISTS events_au AFTER UPDATE OF text, path ON events BEGIN
  INSERT INTO events_fts(events_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
  INSERT INTO events_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
  INSERT INTO events_fts(events_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
END;

-- Distilled memories: typed observations and session summaries.
-- Id ranges: < 1,000,000 are claude-mem observation ids kept verbatim (so ids cited in
-- old notes still resolve); 1,000,000 + n are claude-mem summary n; anything newer is
-- allocated above every existing id.
CREATE TABLE IF NOT EXISTS memories(
  id INTEGER PRIMARY KEY,
  session_id TEXT,
  project TEXT,
  kind TEXT NOT NULL,                -- observation | summary
  type TEXT,                         -- bugfix, feature, decision, discovery, ...
  title TEXT,
  subtitle TEXT,
  narrative TEXT,
  facts TEXT,                        -- JSON array
  concepts TEXT,                     -- JSON array
  files_read TEXT,                   -- JSON array
  files_modified TEXT,               -- JSON array
  data TEXT,                         -- JSON: original structured fields
  origin TEXT NOT NULL,              -- claude-mem | mnem
  origin_id TEXT NOT NULL,
  model TEXT,
  created_at INTEGER,
  UNIQUE(origin, origin_id)
);
CREATE INDEX IF NOT EXISTS memories_project ON memories(project, created_at);
CREATE INDEX IF NOT EXISTS memories_session ON memories(session_id);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  title, subtitle, narrative, facts, concepts,
  content='memories', content_rowid='id', tokenize='porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, title, subtitle, narrative, facts, concepts)
  VALUES (new.id, new.title, new.subtitle, new.narrative, new.facts, new.concepts);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, title, subtitle, narrative, facts, concepts)
  VALUES ('delete', old.id, old.title, old.subtitle, old.narrative, old.facts, old.concepts);
END;

-- Which transcript events support each distilled memory. event_hash is the event's
-- text hash when linked, so a later rewrite of that event shows as changed evidence.
CREATE TABLE IF NOT EXISTS memory_evidence(
  memory_id INTEGER NOT NULL,
  event_id INTEGER NOT NULL,
  event_hash TEXT,
  relation TEXT NOT NULL DEFAULT 'cited',
  PRIMARY KEY(memory_id, event_id)
);
CREATE INDEX IF NOT EXISTS memory_evidence_event ON memory_evidence(event_id);

-- Per-session high-water mark of events already distilled into memories.
CREATE TABLE IF NOT EXISTS distill_state(
  session_id TEXT PRIMARY KEY,
  through INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER,
  error TEXT
);

-- What each session has already been shown of each other session (cross-agent delta).
-- Sessions not shown yet keep their mark and appear on a later prompt.
CREATE TABLE IF NOT EXISTS delta_seen(
  viewer TEXT NOT NULL,
  other TEXT NOT NULL,
  through INTEGER NOT NULL,
  PRIMARY KEY(viewer, other)
);

-- Tombstones for forgotten data, keyed by ids that survive re-ingest and re-import:
-- memory = origin:origin_id, event = session|record_key, session = id, project = id.
CREATE TABLE IF NOT EXISTS forgotten(
  kind TEXT NOT NULL,
  key TEXT NOT NULL,
  at INTEGER NOT NULL,
  PRIMARY KEY(kind, key)
);

-- Memories already offered to a session by prompt-time recall (never repeated).
CREATE TABLE IF NOT EXISTS recall_seen(
  session_id TEXT NOT NULL,
  memory_id INTEGER NOT NULL,
  PRIMARY KEY(session_id, memory_id)
);

-- Per-session floor: events before the session started are never "meanwhile" news.
CREATE TABLE IF NOT EXISTS injections(
  session_id TEXT PRIMARY KEY,
  watermark INTEGER NOT NULL
);

-- Lines that failed to parse. Kept for replay once an adapter learns the format.
CREATE TABLE IF NOT EXISTS quarantine(
  source_path TEXT NOT NULL,
  byte_offset INTEGER NOT NULL,
  generation INTEGER NOT NULL DEFAULT 0,
  reason TEXT,
  line TEXT,
  PRIMARY KEY(source_path, generation, byte_offset)
);
"#;

pub fn data_dir() -> PathBuf {
    std::env::var_os("MNEM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".mnem"))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Bump whenever SCHEMA or `migrate` changes; an up-to-date database then opens
/// without taking a write lock.
const SCHEMA_VERSION: i64 = 10;

pub fn open(path: &Path) -> Result<Connection> {
    open_with(path, Duration::from_secs(5))
}

/// Open with a caller-chosen lock wait. Hooks use a short one so an agent's turn never
/// stalls behind a long write.
pub fn open_with(path: &Path, busy: Duration) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(busy)?;
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(conn)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Migrations for databases created by older builds.
fn migrate(conn: &Connection) -> Result<()> {
    // quarantine gained a generation column in its primary key; it only holds diagnostics.
    let has_gen: bool = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('quarantine') WHERE name = 'generation'",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !has_gen {
        conn.execute_batch("DROP TABLE quarantine;")?;
        conn.execute_batch(SCHEMA)?;
    }
    for (table, col, decl) in [
        ("sources", "checkpoint", "TEXT"),
        ("sources", "file_id", "TEXT"),
        ("sources", "mtime_seen", "INTEGER"),
        ("events", "thread", "TEXT"),
        ("events", "label", "TEXT"),
        ("events", "tool_raw", "TEXT"),
    ] {
        let has: bool = conn.query_row(
            &format!("SELECT count(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            [col],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS events_turn ON events(session_id, turn, kind);",
    )?;
    Ok(())
}
