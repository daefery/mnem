//! Incremental transcript ingest.
//!
//! Parsing happens outside any transaction. The commit re-checks the cursor under
//! `BEGIN IMMEDIATE`, then writes events, session metadata and the advanced cursor
//! atomically. Events are deduped by (session, agent record key), so replaying a file
//! after a rewrite or a lost race is harmless.

use crate::adapters;
use crate::db;
use crate::model::{Agent, Event, ParserState};
use crate::project::Resolver;
use crate::text;
use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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
    pub state: ParserState,
}

pub struct Batch {
    pub src: Source,
    pub base: (u64, i64),
    pub generation: i64,
    pub end: u64,
    pub size: u64,
    pub fingerprint: String,
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
        for e in WalkDir::new(&root).follow_links(false).into_iter().filter_map(Result::ok) {
            if e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl") {
                out.push(Source { path: e.into_path(), agent });
            }
        }
    }
    out
}

pub fn agent_for(path: &Path) -> Option<Agent> {
    roots().into_iter().find(|(root, _)| path.starts_with(root)).map(|(_, a)| a)
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

pub fn load_cursor(conn: &Connection, path: &Path) -> Result<Option<Cursor>> {
    conn.query_row(
        "SELECT byte_offset, generation, fingerprint, parser_state FROM sources WHERE path = ?1",
        params![path.to_string_lossy()],
        |r| {
            let state: Option<String> = r.get(3)?;
            Ok(Cursor {
                offset: r.get::<_, i64>(0)? as u64,
                generation: r.get(1)?,
                fingerprint: r.get(2)?,
                state: state.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn fingerprint(f: &mut std::fs::File) -> Result<Option<String>> {
    f.seek(SeekFrom::Start(0))?;
    let mut first = Vec::new();
    BufReader::new(f.take(4096)).read_until(b'\n', &mut first)?;
    Ok((!first.is_empty()).then(|| text::hash(&String::from_utf8_lossy(&first))))
}

/// Parse new complete lines past the cursor. None when there is nothing new.
pub fn parse(src: &Source, cur: Option<Cursor>) -> Result<Option<Batch>> {
    let mut f = std::fs::File::open(&src.path).with_context(|| format!("open {}", src.path.display()))?;
    let size = f.metadata()?.len();
    let Some(fp) = fingerprint(&mut f)? else { return Ok(None) };
    let base = cur.as_ref().map(|c| (c.offset, c.generation)).unwrap_or((0, 0));
    // A different first line or a shrunk file means the transcript was rewritten.
    let (cur, generation) = match cur {
        Some(c) if c.fingerprint.as_deref() == Some(fp.as_str()) && size >= c.offset => {
            let g = c.generation;
            (c, g)
        }
        Some(c) => (Cursor::default(), c.generation + 1),
        None => (Cursor::default(), 0),
    };
    if size == cur.offset && generation == base.1 {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(cur.offset))?;
    let mut buf = Vec::with_capacity((size - cur.offset) as usize);
    f.take(size - cur.offset).read_to_end(&mut buf)?;
    // Only consume complete, newline-terminated records; a partial tail waits for the writer.
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(None);
    };
    let mut state = cur.state;
    let mut events = Vec::new();
    let mut bad = Vec::new();
    let mut pos = 0usize;
    for raw in buf[..=last_nl].split(|&b| b == b'\n') {
        let off = cur.offset + pos as u64;
        pos += raw.len() + 1;
        let line = String::from_utf8_lossy(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Err(e) = adapters::parse_line(src.agent, &mut state, line, off, &mut events) {
            bad.push((off, e, text::head(line, 2000)));
        }
    }
    Ok(Some(Batch {
        src: src.clone(),
        base,
        generation,
        end: cur.offset + last_nl as u64 + 1,
        size,
        fingerprint: fp,
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
        return Ok(CommitStats { stale: true, ..Default::default() });
    }
    let st = &b.state;
    let native = st.session_id.clone().unwrap_or_else(|| {
        b.src.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
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
            "INSERT OR IGNORE INTO events(session_id, record_key, ts, turn, kind, tool, path, text, is_error, source_path, byte_offset)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            ])?;
        }
    }
    {
        let mut q = tx.prepare_cached(
            "INSERT OR IGNORE INTO quarantine(source_path, byte_offset, reason, line) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (off, reason, line) in &b.bad {
            q.execute(params![path, *off as i64, reason, line])?;
        }
    }
    tx.execute(
        "INSERT INTO sources(path, agent, fingerprint, generation, byte_offset, size_seen, parser_state, session_id, excluded, bad_lines, last_ingest_at, missing_since)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)
         ON CONFLICT(path) DO UPDATE SET
           fingerprint = excluded.fingerprint, generation = excluded.generation,
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
        ],
    )?;
    tx.commit()?;
    Ok(CommitStats { inserted, stale: false })
}

/// Catch up one transcript (hook hot path).
pub fn ingest_file(conn: &mut Connection, src: &Source, resolver: &mut Resolver) -> Result<CommitStats> {
    let cur = load_cursor(conn, &src.path)?;
    match parse(src, cur)? {
        Some(b) => commit(conn, &b, resolver),
        None => Ok(CommitStats::default()),
    }
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
    let mut stats = SweepStats { files: sources.len(), ..Default::default() };
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
    for r in rx {
        match r {
            Ok(b) => {
                stats.changed += 1;
                stats.events += b.events.len();
                stats.bad_lines += b.bad.len();
                let c = commit(conn, &b, &mut resolver)?;
                stats.inserted += c.inserted;
                stats.stale += c.stale as usize;
            }
            Err(e) => stats.errors.push(e),
        }
    }
    producer.join().map_err(|_| anyhow::anyhow!("parser thread panicked"))?;
    Ok(stats)
}

/// Stamp transcripts that vanished (agent cleanup) so doctor can report the loss window.
/// `present` must be the full discovery result, not a subset.
pub fn mark_missing(conn: &Connection, present: &[Source]) -> Result<usize> {
    let seen: Vec<String> = present.iter().map(|s| s.path.to_string_lossy().into_owned()).collect();
    Ok(conn.execute(
        "UPDATE sources SET missing_since = coalesce(missing_since, ?1)
         WHERE path NOT IN (SELECT value FROM json_each(?2))",
        params![db::now_ms(), serde_json::to_string(&seen)?],
    )?)
}
