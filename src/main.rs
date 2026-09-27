use anyhow::Result;
use clap::{Parser, Subcommand};
use mnem::{db, doctor, import, ingest, project, search};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "mnem",
    version,
    about = "Local-first memory for coding agents, built from their own transcripts"
)]
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
    /// Import a claude-mem database (read-only snapshot; safe to re-run)
    Import {
        #[arg(long)]
        from: Option<PathBuf>,
    },
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
            let bytes: u64 = sources
                .iter()
                .filter_map(|s| s.path.metadata().ok())
                .map(|m| m.len())
                .sum();
            let stats = ingest::sweep(&mut conn, sources.clone())?;
            let missing = ingest::mark_missing(&conn, &sources)?;
            let secs = t.elapsed().as_secs_f64();
            println!(
                "backfill: {} files ({:.0} MB), {} changed, {} events parsed, {} inserted, {} bad lines, {} still behind, {} newly missing, {:.1}s ({:.0} MB/s)",
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
        Cmd::Import { from } => {
            let src = from.unwrap_or_else(|| db::home().join(".claude-mem/claude-mem.db"));
            let t = Instant::now();
            let s = import::claude_mem(&mut conn, &src)?;
            println!(
                "import: {} claude-mem sessions ({} new), {} observations, {} summaries, {} prompts ({} skipped: transcript present), {} project names mapped, {:.1}s",
                s.sessions_seen,
                s.sessions_added,
                s.observations,
                s.summaries,
                s.prompts,
                s.prompts_skipped,
                s.projects_mapped,
                t.elapsed().as_secs_f64()
            );
        }
        Cmd::Ingest { path } => {
            let path = std::fs::canonicalize(&path)?;
            let agent = ingest::agent_for(&path).ok_or_else(|| {
                anyhow::anyhow!("{} is not under a known transcript root", path.display())
            })?;
            let mut resolver = project::Resolver::default();
            let o = ingest::ingest_file(&mut conn, &ingest::Source { path, agent }, &mut resolver)?;
            println!("ingest: {} inserted, {:?}", o.inserted, o.status);
            if o.status != ingest::Status::CaughtUp {
                std::process::exit(75); // EX_TEMPFAIL
            }
        }
        Cmd::Doctor { strict } => {
            let healthy = doctor::run(&conn)?;
            if strict && !healthy {
                std::process::exit(1);
            }
        }
        Cmd::Search {
            query,
            project,
            limit,
        } => {
            search::run(&conn, &query.join(" "), project.as_deref(), limit)?;
        }
    }
    Ok(())
}
