//! Incremental transcript ingest.
//!
//! Parsing happens outside any transaction. The commit re-checks the cursor under
//! `BEGIN IMMEDIATE`, then writes events, session metadata and the advanced cursor
//! atomically. Events are deduped by (session, agent record key), so replaying a file
//! after a rewrite or a lost race is harmless.

use crate::adapters;
use crate::db;
use crate::model::{Agent, Event, ParserState, STATE_VERSION};
use crate::project::Resolver;
use crate::text;
use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct Source {
    pub path: PathBuf,
    pub agent: Agent,
}

#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub offset: u64,
    pub generation: i64,
    pub fingerprint: Option<String>,
    pub checkpoint: Option<String>,
    pub file_id: Option<String>,
    /// None when missing or written by an incompatible build: forces a replay from 0.
    pub state: Option<ParserState>,
}

pub struct Batch {
    pub src: Source,
    /// (offset, generation) as stored when parsing began; commit is refused if it moved.
    pub base: (u64, i64),
    pub generation: i64,
    pub end: u64,
    pub size: u64,
    pub fingerprint: String,
    pub checkpoint: Option<String>,
    pub file_id: Option<String>,
    pub state: ParserState,
    pub events: Vec<Event>,
    pub bad: Vec<(u64, String, String)>,
}

pub fn roots() -> Vec<(PathBuf, Agent)> {
    let h = db::home();
    vec![
        (h.join(".claude/projects"), Agent::Claude),
        (h.join(".codex/sessions"), Agent::Codex),
        (h.join(".pi/agent/sessions"), Agent::Pi),
    ]
}

pub fn discover() -> Vec<Source> {
    let mut out = Vec::new();
    for (root, agent) in roots() {
        for e in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl") {
                out.push(Source {
                    path: e.into_path(),
                    agent,
                });
            }
        }
    }
    out
}

pub fn agent_for(path: &Path) -> Option<Agent> {
    roots()
        .into_iter()
        .find(|(root, _)| path.starts_with(root))
        .map(|(_, a)| a)
}

/// claude-mem's own observer runs are Claude Code sessions too; they are not user work.
pub fn is_excluded(path: &Path, cwd: Option<&str>) -> bool {
    let p = path.to_string_lossy();
    if p.contains("claude-mem-observer-sessions") {
        return true;
    }
    let cm = db::home().join(".claude-mem");
    cwd.is_some_and(|c| Path::new(c).starts_with(&cm))
}

/// Claude Code writes subagent runs to <session>/subagents/agent-<id>.jsonl.
fn subagent_thread(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    (parent.file_name()? == "subagents").then(|| path.file_stem()?.to_str().map(str::to_string))?
}

pub fn load_cursor(conn: &Connection, path: &Path) -> Result<Option<Cursor>> {
    conn.query_row(
        "SELECT byte_offset, generation, fingerprint, checkpoint, file_id, parser_state FROM sources WHERE path = ?1",
        params![path.to_string_lossy()],
        |r| {
            let state: Option<String> = r.get(5)?;
            Ok(Cursor {
                offset: r.get::<_, i64>(0)? as u64,
                generation: r.get(1)?,
                fingerprint: r.get(2)?,
                checkpoint: r.get(3)?,
                file_id: r.get(4)?,
                state: state
                    .and_then(|s| serde_json::from_str::<ParserState>(&s).ok())
                    .filter(|s| s.v == STATE_VERSION),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

const CHECKPOINT_BYTES: u64 = 4096;
/// Larger records are quarantined instead of parsed (real lines reach ~5 MB).
const MAX_LINE: usize = 64 * 1024 * 1024;

fn fingerprint(f: &mut File) -> Result<Option<String>> {
    f.seek(SeekFrom::Start(0))?;
    let mut first = Vec::new();
    BufReader::new(f.take(4096)).read_until(b'\n', &mut first)?;
    Ok((!first.is_empty()).then(|| text::hash(&String::from_utf8_lossy(&first))))
}

/// Hash of the bytes just before `offset`: proves the already-consumed prefix is unchanged.
fn checkpoint(f: &mut File, offset: u64) -> Result<Option<String>> {
    if offset == 0 {
        return Ok(None);
    }
    let start = offset.saturating_sub(CHECKPOINT_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((offset - start) as usize);
    f.take(offset - start).read_to_end(&mut buf)?;
    Ok(Some(text::hash(&String::from_utf8_lossy(&buf))))
}

#[cfg(unix)]
fn file_id(m: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_: &std::fs::Metadata) -> Option<String> {
    None
}

/// Parse new complete lines past the cursor. None when there is nothing new.
///
/// Resumes only when the file provably still holds what was consumed: same first line,
/// same file identity, not shrunk, same checkpoint bytes, compatible parser state.
/// Anything else replays the file from 0 under a new generation; dedupe on insert keeps
/// already-stored events and adds the changed ones.
pub fn parse(src: &Source, cur: Option<Cursor>) -> Result<Option<Batch>> {
    let mut f = File::open(&src.path).with_context(|| format!("open {}", src.path.display()))?;
    let meta = f.metadata()?;
    let size = meta.len();
    let fid = file_id(&meta);
    let Some(fp) = fingerprint(&mut f)? else {
        return Ok(None);
    };
    let base = cur
        .as_ref()
        .map(|c| (c.offset, c.generation))
        .unwrap_or((0, 0));
    let (start, generation, mut state) = match cur {
        Some(c) => {
            let same = c.fingerprint.as_deref() == Some(fp.as_str())
                && (c.file_id.is_none() || c.file_id == fid)
                && size >= c.offset
                && c.state.is_some()
                && checkpoint(&mut f, c.offset)? == c.checkpoint;
            match c.state {
                Some(st) if same => (c.offset, c.generation, st),
                _ => (0, c.generation + 1, ParserState::default()),
            }
        }
        None => (0, 0, ParserState::default()),
    };
    if size == start && generation == base.1 {
        return Ok(None);
    }
    if start == 0 {
        state.thread = subagent_thread(&src.path);
    }

    f.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(1 << 20, (&mut f).take(size - start));
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut bad = Vec::new();
    let mut pos = start;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        // Only consume complete, newline-terminated records; a partial tail waits.
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let off = pos;
        pos += n as u64;
        if n > MAX_LINE {
            bad.push((off, format!("oversized record: {n} bytes"), String::new()));
            continue;
        }
        let s = String::from_utf8_lossy(&line);
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if let Err(e) = adapters::parse_line(src.agent, &mut state, s, off, &mut events) {
            bad.push((off, e, text::clean(s, 2000)));
        }
    }
    drop(reader);
    if pos == start && generation == base.1 {
        return Ok(None);
    }
    Ok(Some(Batch {
        src: src.clone(),
        base,
        generation,
        end: pos,
        size,
        fingerprint: fp,
        checkpoint: checkpoint(&mut f, pos)?,
        file_id: fid,
        state,
        events,
        bad,
    }))
}

#[derive(Default, Debug)]
pub struct CommitStats {
    pub inserted: usize,
    pub stale: bool,
}

pub fn commit(conn: &mut Connection, b: &Batch, resolver: &mut Resolver) -> Result<CommitStats> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let path = b.src.path.to_string_lossy().into_owned();
    let now = db::now_ms();
    let current: Option<(i64, i64)> = tx
        .query_row(
            "SELECT byte_offset, generation FROM sources WHERE path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if current.map(|(o, g)| (o as u64, g)).unwrap_or((0, 0)) != b.base {
        // Another process advanced this cursor since we parsed; it owns those lines.
        return Ok(CommitStats {
            stale: true,
            ..Default::default()
        });
    }
    let st = &b.state;
    let native = st.session_id.clone().unwrap_or_else(|| {
        b.src
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let sid = format!("{}:{native}", b.src.agent.as_str());
    let excluded = is_excluded(&b.src.path, st.cwd.as_deref());
    let mut inserted = 0;
    if !excluded {
        let project = resolver.resolve(st.cwd.as_deref(), st.repo_url.as_deref());
        tx.execute(
            "INSERT INTO sessions(id, agent, native_id, project, cwd, git_branch, title, started_at, last_event_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
               project = coalesce(excluded.project, project),
               cwd = coalesce(excluded.cwd, cwd),
               git_branch = coalesce(excluded.git_branch, git_branch),
               title = coalesce(excluded.title, title),
               started_at = min(coalesce(started_at, excluded.started_at), coalesce(excluded.started_at, started_at)),
               last_event_at = max(coalesce(last_event_at, 0), coalesce(excluded.last_event_at, 0))",
            params![
                sid,
                b.src.agent.as_str(),
                native,
                project,
                st.cwd,
                st.git_branch,
                st.title,
                st.started_at,
                (st.last_ts > 0).then_some(st.last_ts),
            ],
        )?;
        let mut ins = tx.prepare_cached(
            // Same record key = same agent record. If a rewrite changed its content, the
            // latest version wins; unchanged rows are left alone (and not counted).
            "INSERT INTO events(session_id, record_key, ts, turn, kind, tool, path, text, is_error, source_path, byte_offset, thread, label, tool_raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(session_id, record_key) DO UPDATE SET
               ts = excluded.ts, turn = excluded.turn, kind = excluded.kind, tool = excluded.tool,
               path = excluded.path, text = excluded.text, is_error = excluded.is_error,
               source_path = excluded.source_path, byte_offset = excluded.byte_offset,
               thread = excluded.thread, label = excluded.label, tool_raw = excluded.tool_raw
             WHERE events.text IS NOT excluded.text OR events.path IS NOT excluded.path
                OR events.kind IS NOT excluded.kind OR events.is_error IS NOT excluded.is_error
                OR events.label IS NOT excluded.label OR events.tool IS NOT excluded.tool",
        )?;
        for e in &b.events {
            inserted += ins.execute(params![
                sid,
                e.key,
                e.ts,
                e.turn,
                e.kind.as_str(),
                e.tool,
                e.path,
                e.text,
                e.is_error,
                path,
                e.byte_offset as i64,
                e.thread,
                e.label,
                e.tool_raw,
            ])?;
        }
    }
    {
        let mut q = tx.prepare_cached(
            "INSERT OR IGNORE INTO quarantine(source_path, generation, byte_offset, reason, line)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (off, reason, line) in &b.bad {
            q.execute(params![path, b.generation, *off as i64, reason, line])?;
        }
    }
    tx.execute(
        "INSERT INTO sources(path, agent, fingerprint, generation, byte_offset, size_seen, parser_state, session_id, excluded, bad_lines, last_ingest_at, missing_since, checkpoint, file_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, ?12, ?13)
         ON CONFLICT(path) DO UPDATE SET
           fingerprint = excluded.fingerprint, generation = excluded.generation,
           checkpoint = excluded.checkpoint, file_id = excluded.file_id,
           byte_offset = excluded.byte_offset, size_seen = excluded.size_seen,
           parser_state = excluded.parser_state, session_id = excluded.session_id,
           excluded = excluded.excluded, bad_lines = bad_lines + excluded.bad_lines,
           last_ingest_at = excluded.last_ingest_at, missing_since = NULL",
        params![
            path,
            b.src.agent.as_str(),
            b.fingerprint,
            b.generation,
            b.end as i64,
            b.size as i64,
            serde_json::to_string(st)?,
            sid,
            excluded,
            b.bad.len() as i64,
            now,
            b.checkpoint,
            b.file_id,
        ],
    )?;
    tx.commit()?;
    Ok(CommitStats {
        inserted,
        stale: false,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Every complete record in the file is committed.
    CaughtUp,
    /// Lost the commit race repeatedly; another writer is active. Retry later.
    Behind,
}

#[derive(Debug)]
pub struct Outcome {
    pub inserted: usize,
    pub status: Status,
}

/// Catch up one transcript (hook hot path). Re-parses after losing a commit race, so
/// the caller learns whether the file is really caught up rather than just an insert count.
pub fn ingest_file(
    conn: &mut Connection,
    src: &Source,
    resolver: &mut Resolver,
) -> Result<Outcome> {
    let mut inserted = 0;
    for _ in 0..5 {
        let cur = load_cursor(conn, &src.path)?;
        let Some(b) = parse(src, cur)? else {
            return Ok(Outcome {
                inserted,
                status: Status::CaughtUp,
            });
        };
        inserted += commit(conn, &b, resolver)?.inserted;
    }
    Ok(Outcome {
        inserted,
        status: Status::Behind,
    })
}

#[derive(Default, Debug)]
pub struct SweepStats {
    pub files: usize,
    pub changed: usize,
    pub events: usize,
    pub inserted: usize,
    pub bad_lines: usize,
    pub stale: usize,
    pub errors: Vec<String>,
}

/// Parse every changed transcript in parallel; commit serially on this thread.
pub fn sweep(conn: &mut Connection, sources: Vec<Source>) -> Result<SweepStats> {
    let mut stats = SweepStats {
        files: sources.len(),
        ..Default::default()
    };
    let work: Vec<(Source, Option<Cursor>)> = sources
        .into_iter()
        .map(|s| {
            let c = load_cursor(conn, &s.path)?;
            Ok((s, c))
        })
        .collect::<Result<_>>()?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Batch, String>>(32);
    let producer = std::thread::spawn(move || {
        work.into_par_iter().for_each_with(tx, |tx, (src, cur)| {
            let r = match parse(&src, cur) {
                Ok(Some(b)) => Ok(b),
                Ok(None) => return,
                Err(e) => Err(format!("{}: {e:#}", src.path.display())),
            };
            let _ = tx.send(r);
        });
    });
    let mut resolver = Resolver::default();
    let mut stale = Vec::new();
    for r in rx {
        match r {
            Ok(b) => {
                stats.changed += 1;
                stats.events += b.events.len();
                stats.bad_lines += b.bad.len();
                let c = commit(conn, &b, &mut resolver)?;
                stats.inserted += c.inserted;
                if c.stale {
                    stale.push(b.src);
                }
            }
            Err(e) => stats.errors.push(e),
        }
    }
    producer
        .join()
        .map_err(|_| anyhow::anyhow!("parser thread panicked"))?;
    // Another writer moved these cursors mid-sweep; catch them up from the new position.
    for src in stale {
        let o = ingest_file(conn, &src, &mut resolver)?;
        stats.inserted += o.inserted;
        stats.stale += (o.status == Status::Behind) as usize;
    }
    Ok(stats)
}

/// Stamp transcripts that vanished (agent cleanup) so doctor can report the loss window.
/// `present` must be the full discovery result, not a subset.
pub fn mark_missing(conn: &Connection, present: &[Source]) -> Result<usize> {
    let seen = serde_json::to_string(
        &present
            .iter()
            .map(|s| s.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    )?;
    conn.execute(
        "UPDATE sources SET missing_since = NULL
         WHERE missing_since IS NOT NULL AND path IN (SELECT value FROM json_each(?1))",
        params![seen],
    )?;
    Ok(conn.execute(
        "UPDATE sources SET missing_since = ?1
         WHERE missing_since IS NULL AND path NOT IN (SELECT value FROM json_each(?2))",
        params![db::now_ms(), seen],
    )?)
}
