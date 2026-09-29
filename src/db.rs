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
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF title, subtitle, narrative, facts, concepts ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, title, subtitle, narrative, facts, concepts)
  VALUES ('delete', old.id, old.title, old.subtitle, old.narrative, old.facts, old.concepts);
  INSERT INTO memories_fts(rowid, title, subtitle, narrative, facts, concepts)
  VALUES (new.id, new.title, new.subtitle, new.narrative, new.facts, new.concepts);
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
  error TEXT,
  -- A too-small final chunk left alone after a day idle: not pending any more, but
  -- still read (from `through`) if the session resumes.
  settled INTEGER NOT NULL DEFAULT 0
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

-- Semantic vectors for memories, int8 with a per-vector scale, per embedding model.
CREATE TABLE IF NOT EXISTS memory_vectors(
  memory_id INTEGER NOT NULL,
  model TEXT NOT NULL,
  dim INTEGER NOT NULL,
  scale REAL NOT NULL,
  vec BLOB NOT NULL,
  -- Hash of the exact text embedded; backfill re-checks it against the memory at commit.
  text_hash TEXT,
  PRIMARY KEY(memory_id, model)
);

-- A vector describes the text it was made from: drop it when the memory goes away or
-- its text changes (the watcher re-embeds). Memory ids can be reused after a delete.
CREATE TRIGGER IF NOT EXISTS memories_vec_ad AFTER DELETE ON memories BEGIN
  DELETE FROM memory_vectors WHERE memory_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS memories_vec_au AFTER UPDATE OF title, subtitle, narrative, facts ON memories BEGIN
  DELETE FROM memory_vectors WHERE memory_id = old.id;
END;

-- The files each memory read or modified, one row per file, kept in step with the
-- memory's JSON lists by triggers; `name` (the last path component) is what lookups
-- start from, the full path decides.
CREATE TABLE IF NOT EXISTS memory_files(
  memory_id INTEGER NOT NULL,
  modified INTEGER NOT NULL,
  path TEXT NOT NULL,
  name TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS memory_files_name ON memory_files(name);
CREATE INDEX IF NOT EXISTS memory_files_memory ON memory_files(memory_id);
CREATE TRIGGER IF NOT EXISTS memories_files_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memory_files(memory_id, modified, path, name)
    SELECT new.id, 1, value, replace(replace(value, char(92), '/'), rtrim(replace(value, char(92), '/'), replace(replace(value, char(92), '/'), '/', '')), '')
      FROM json_each(CASE WHEN json_valid(new.files_modified) THEN new.files_modified ELSE '[]' END)
     WHERE json_each.type = 'text' AND value != ''
    UNION ALL
    SELECT new.id, 0, value, replace(replace(value, char(92), '/'), rtrim(replace(value, char(92), '/'), replace(replace(value, char(92), '/'), '/', '')), '')
      FROM json_each(CASE WHEN json_valid(new.files_read) THEN new.files_read ELSE '[]' END)
     WHERE json_each.type = 'text' AND value != '';
END;
CREATE TRIGGER IF NOT EXISTS memories_files_au AFTER UPDATE OF files_read, files_modified ON memories BEGIN
  DELETE FROM memory_files WHERE memory_id = old.id;
  INSERT INTO memory_files(memory_id, modified, path, name)
    SELECT new.id, 1, value, replace(replace(value, char(92), '/'), rtrim(replace(value, char(92), '/'), replace(replace(value, char(92), '/'), '/', '')), '')
      FROM json_each(CASE WHEN json_valid(new.files_modified) THEN new.files_modified ELSE '[]' END)
     WHERE json_each.type = 'text' AND value != ''
    UNION ALL
    SELECT new.id, 0, value, replace(replace(value, char(92), '/'), rtrim(replace(value, char(92), '/'), replace(replace(value, char(92), '/'), '/', '')), '')
      FROM json_each(CASE WHEN json_valid(new.files_read) THEN new.files_read ELSE '[]' END)
     WHERE json_each.type = 'text' AND value != '';
END;
CREATE TRIGGER IF NOT EXISTS memories_files_ad AFTER DELETE ON memories BEGIN
  DELETE FROM memory_files WHERE memory_id = old.id;
END;

-- Uptake: what mnem put in front of agents, what they asked mnem for, and what the
-- hooks cost (`mnem uptake`). Kept locally like everything else.
CREATE TABLE IF NOT EXISTS offers(
  session_id TEXT NOT NULL,
  memory_id INTEGER NOT NULL,
  source TEXT NOT NULL,              -- start | prompt | file
  at INTEGER NOT NULL,
  PRIMARY KEY(session_id, memory_id, source)
);
CREATE INDEX IF NOT EXISTS offers_at ON offers(at);
CREATE TABLE IF NOT EXISTS mcp_calls(
  id INTEGER PRIMARY KEY,
  at INTEGER NOT NULL,
  tool TEXT NOT NULL,
  project TEXT,
  ids TEXT                           -- JSON array of memory ids asked for, if any
);
CREATE INDEX IF NOT EXISTS mcp_calls_at ON mcp_calls(at);
CREATE TABLE IF NOT EXISTS hook_runs(
  at INTEGER NOT NULL,
  agent TEXT NOT NULL,
  event TEXT NOT NULL,
  ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS hook_runs_at ON hook_runs(at);

-- Every digest sent to the model chain, for the watcher's daily budget and doctor.
CREATE TABLE IF NOT EXISTS distill_calls(
  at INTEGER NOT NULL,
  source TEXT NOT NULL,              -- session (Stop hook), watch, backfill, manual
  model TEXT,
  ok INTEGER NOT NULL,
  requests INTEGER NOT NULL DEFAULT 1 -- HTTP requests it took, fallbacks included
);
CREATE INDEX IF NOT EXISTS distill_calls_at ON distill_calls(at);

-- Sessions driven by another agent's brief (settings: scripted_sessions).
CREATE TABLE IF NOT EXISTS scripted_sessions(
  session_id TEXT PRIMARY KEY,
  at INTEGER NOT NULL
);

-- Files a session has already been offered memories about (once per file).
CREATE TABLE IF NOT EXISTS file_seen(
  session_id TEXT NOT NULL,
  path TEXT NOT NULL,
  PRIMARY KEY(session_id, path)
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
pub const SCHEMA_VERSION: i64 = 22;

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

/// Make a database from elsewhere safe to adopt: drop every trigger, view and index and
/// every table this build does not define, then recreate the schema objects from this
/// build. A crafted file can then carry only data, never code that runs on later writes.
pub fn sanitize_schema(conn: &Connection) -> Result<()> {
    let known: Vec<String> = {
        let fresh = Connection::open_in_memory()?;
        fresh.execute_batch(SCHEMA)?;
        migrate(&fresh)?;
        let mut st = fresh.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
        st.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let objects: Vec<(String, String)> = {
        let mut st =
            conn.prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let quote = |n: &str| format!("\"{}\"", n.replace('"', "\"\""));
    // Triggers and views first: they may reference the tables dropped below.
    for kind in ["trigger", "view", "index"] {
        for (t, name) in objects.iter().filter(|(t, _)| t == kind) {
            conn.execute_batch(&format!(
                "DROP {} IF EXISTS {}",
                t.to_uppercase(),
                quote(name)
            ))?;
        }
    }
    for (_, name) in objects
        .iter()
        .filter(|(t, n)| t == "table" && !known.contains(n))
    {
        // Shadow tables of an unknown virtual table go with it.
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", quote(name)))?;
    }
    conn.execute_batch(SCHEMA)?;
    migrate(conn)?;
    Ok(())
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
        ("memory_vectors", "text_hash", "TEXT"),
        ("distill_state", "settled", "INTEGER NOT NULL DEFAULT 0"),
        ("distill_calls", "requests", "INTEGER NOT NULL DEFAULT 1"),
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
    // memory_vectors was first keyed by memory_id alone, so a second model overwrote the
    // first. Rebuild it keyed by (memory_id, model); vectors are cheap to recompute.
    let keyed_by_model: bool = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('memory_vectors') WHERE name = 'model' AND pk > 0",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !keyed_by_model {
        conn.execute_batch(
            "DROP TABLE IF EXISTS memory_vectors;
             CREATE TABLE memory_vectors(memory_id INTEGER NOT NULL, model TEXT NOT NULL, dim INTEGER NOT NULL,
               scale REAL NOT NULL, vec BLOB NOT NULL, text_hash TEXT, PRIMARY KEY(memory_id, model));",
        )?;
    }
    // memory_files arrived after most memories: fill it from their file lists, and
    // again whenever how names are taken changes (v2: Windows separators too). The
    // triggers are recreated with it.
    let version: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'memory_files.v'", [], |r| {
            r.get(0)
        })
        .ok();
    if version.as_deref() != Some("2") {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS memories_files_ai;
             DROP TRIGGER IF EXISTS memories_files_au;
             DROP TRIGGER IF EXISTS memories_files_ad;
             DELETE FROM memory_files;",
        )?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO memory_files(memory_id, modified, path, name)
               SELECT m.id, 1, j.value, replace(replace(j.value, char(92), '/'), rtrim(replace(j.value, char(92), '/'), replace(replace(j.value, char(92), '/'), '/', '')), '')
                 FROM memories m, json_each(CASE WHEN json_valid(m.files_modified) THEN m.files_modified ELSE '[]' END) j
                WHERE j.type = 'text' AND j.value != ''
               UNION ALL
               SELECT m.id, 0, j.value, replace(replace(j.value, char(92), '/'), rtrim(replace(j.value, char(92), '/'), replace(replace(j.value, char(92), '/'), '/', '')), '')
                 FROM memories m, json_each(CASE WHEN json_valid(m.files_read) THEN m.files_read ELSE '[]' END) j
                WHERE j.type = 'text' AND j.value != '';",
        )?;
        conn.execute(
            "INSERT INTO meta(k, v) VALUES ('memory_files.v', '2') ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            [],
        )?;
    }
    Ok(())
}
