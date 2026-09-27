use anyhow::Result;
use mnem::{db, doctor, ingest, project, search};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "mnem", version, about = "Local-first memory for coding agents, built from their own transcripts")]
struct Cli {
    /// Database path (default: $MNEM_HOME/mnem.db or ~/.mnem/mnem.db)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ingest every Claude Code, Codex and pi transcript (incremental; safe to re-run)
    Backfill,
    /// Catch up a single transcript file
    Ingest { path: PathBuf },
    /// Capture health: coverage, lag, quarantine, claude-mem comparison
    Doctor {
        /// Exit non-zero when any live transcript has unread bytes
        #[arg(long)]
        strict: bool,
    },
    /// Full-text search over captured events
    Search {
        query: Vec<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.db.unwrap_or_else(|| db::data_dir().join("mnem.db"));
    let mut conn = db::open(&path)?;
    match cli.cmd {
        Cmd::Backfill => {
            let t = Instant::now();
            let sources = ingest::discover();
            let bytes: u64 = sources.iter().filter_map(|s| s.path.metadata().ok()).map(|m| m.len()).sum();
            let stats = ingest::sweep(&mut conn, sources.clone())?;
            let missing = ingest::mark_missing(&conn, &sources)?;
            let secs = t.elapsed().as_secs_f64();
            println!(
                "backfill: {} files ({:.0} MB), {} changed, {} events parsed, {} inserted, {} bad lines, {} stale, {} newly missing, {:.1}s ({:.0} MB/s)",
                stats.files,
                bytes as f64 / 1e6,
                stats.changed,
                stats.events,
                stats.inserted,
                stats.bad_lines,
                stats.stale,
                missing,
                secs,
                bytes as f64 / 1e6 / secs.max(1e-9),
            );
            for e in stats.errors.iter().take(10) {
                eprintln!("error: {e}");
            }
        }
        Cmd::Ingest { path } => {
            let path = std::fs::canonicalize(&path)?;
            let agent = ingest::agent_for(&path)
                .ok_or_else(|| anyhow::anyhow!("{} is not under a known transcript root", path.display()))?;
            let mut resolver = project::Resolver::default();
            let s = ingest::ingest_file(&mut conn, &ingest::Source { path, agent }, &mut resolver)?;
            println!("ingest: {} inserted{}", s.inserted, if s.stale { " (stale, retry)" } else { "" });
        }
        Cmd::Doctor { strict } => {
            let healthy = doctor::run(&conn)?;
            if strict && !healthy {
                std::process::exit(1);
            }
        }
        Cmd::Search { query, project, limit } => {
            search::run(&conn, &query.join(" "), project.as_deref(), limit)?;
        }
    }
    Ok(())
}
