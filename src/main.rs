use anyhow::Result;
use clap::{Parser, Subcommand};
use mnem::model::Agent;
use mnem::{context, db, distill, doctor, hook, import, ingest, install, mcp, project, search};
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
    /// Agent hook entry point; reads the hook JSON on stdin. Never fails the agent.
    Hook {
        agent: Agent,
        /// session-start | prompt | stop | session-end
        event: String,
    },
    /// Print the session-start context for a project (what hooks inject)
    Context {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        project: Option<String>,
        /// Caller's mnem session id, excluded from "recent" (e.g. claude:<uuid>)
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 8000)]
        budget: usize,
    },
    /// Wire mnem into Claude Code, Codex and pi (backs up every file it changes)
    Install {
        /// Print the plan without writing anything
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated subset: claude,codex,pi
        #[arg(long, default_value = "claude,codex,pi")]
        only: String,
        /// mnem binary to register (default: this executable)
        #[arg(long)]
        bin: Option<String>,
        /// Also install and start the `mnem watch` systemd user service
        #[arg(long)]
        watch: bool,
    },
    /// Remove mnem's hooks, MCP entries and pi extension (keeps the database)
    Uninstall {
        #[arg(long)]
        dry_run: bool,
    },
    /// Cross-agent update for a session since it last looked (used by the pi extension)
    Delta {
        #[arg(long)]
        session: String,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Call an MCP tool from the shell: mnem tool search '{"query":"..."}'
    Tool {
        name: String,
        #[arg(default_value = "{}")]
        args: String,
    },
    /// Distil captured events into typed observations and summaries (LLM, off the write path)
    Distill {
        /// Only this mnem session id (e.g. claude:<uuid>)
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 7)]
        since_days: i64,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Show the digests that would be sent, without calling the LLM
        #[arg(long)]
        dry_run: bool,
        /// Include sessions active in the last two minutes
        #[arg(long)]
        active: bool,
        #[arg(long)]
        quiet: bool,
    },
    /// Keep capture and distillation current in the background (runs as a user service)
    Watch {
        /// Seconds between transcript reconciliations
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Seconds between distillation passes over idle sessions (0 = never)
        #[arg(long, default_value_t = 300)]
        distill_every: u64,
    },
    /// MCP server over stdio (search, timeline, get_observations, session_start_context)
    Mcp,
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
    // Paths that run inside an agent's turn must fail fast rather than wait on a lock.
    let in_turn = matches!(
        cli.cmd,
        Cmd::Hook { .. } | Cmd::Delta { .. } | Cmd::Context { .. }
    );
    let busy = std::time::Duration::from_millis(if in_turn { 500 } else { 5000 });
    let mut conn = match db::open_with(&path, busy) {
        Ok(c) => c,
        // A hook must never fail the agent, not even when the database is unavailable.
        Err(e) if matches!(cli.cmd, Cmd::Hook { .. }) => {
            hook::log(&format!("open {}: {e:#}", path.display()));
            return Ok(());
        }
        Err(e) => return Err(e),
    };
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
        Cmd::Hook { agent, event } => {
            let t = Instant::now();
            if let Err(e) = hook::run(&mut conn, agent, &event) {
                hook::log(&format!("{} {event}: {e:#}", agent.as_str()));
            }
            hook::log(&format!(
                "{} {event} took {} ms",
                agent.as_str(),
                t.elapsed().as_millis()
            ));
        }
        Cmd::Context {
            cwd,
            project,
            session,
            budget,
        } => {
            let cwd = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            let project = project
                .or_else(|| hook::project_for(&conn, session.as_deref(), cwd.as_deref()))
                .ok_or_else(|| anyhow::anyhow!("cannot resolve a project; pass --project"))?;
            let fresh = hook::catch_up_recent(&mut conn, std::time::Duration::from_millis(400))?;
            let ctx = context::build(
                &conn,
                &context::Options {
                    project: &project,
                    current: session.as_deref(),
                    budget_chars: budget,
                    sessions: 5,
                    turns: 3,
                    observations: 30,
                },
            )?;
            println!("{ctx}\n---\n{}", fresh.footer(&conn));
        }
        Cmd::Mcp => mcp::serve(&conn)?,
        Cmd::Watch {
            interval,
            distill_every,
        } => {
            let mut last_distill = Instant::now();
            loop {
                let sources = ingest::discover();
                match ingest::sweep(&mut conn, sources.clone()) {
                    Ok(s) => {
                        if s.changed > 0 || !s.errors.is_empty() {
                            hook::log(&format!(
                                "watch: {} changed, {} inserted, {} still behind, {} errors",
                                s.changed,
                                s.inserted,
                                s.stale,
                                s.errors.len()
                            ));
                        }
                        let _ = ingest::mark_missing(&conn, &sources);
                    }
                    Err(e) => hook::log(&format!("watch sweep: {e:#}")),
                }
                if distill_every > 0 && last_distill.elapsed().as_secs() >= distill_every {
                    last_distill = Instant::now();
                    let o = distill::Options {
                        session: None,
                        since_days: 2,
                        limit: 5,
                        dry_run: false,
                        include_active: false,
                    };
                    match distill::run(&mut conn, &o) {
                        Ok(s) if s.calls > 0 || !s.errors.is_empty() => hook::log(&format!(
                            "watch distill: {} calls, {} observations, {} errors",
                            s.calls,
                            s.observations,
                            s.errors.len()
                        )),
                        Ok(_) => {}
                        Err(e) => hook::log(&format!("watch distill: {e:#}")),
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(interval.max(5)));
            }
        }
        Cmd::Distill {
            session,
            since_days,
            limit,
            dry_run,
            active,
            quiet,
        } => {
            let t = Instant::now();
            let s = distill::run(
                &mut conn,
                &distill::Options {
                    session,
                    since_days,
                    limit,
                    dry_run,
                    include_active: active,
                },
            )?;
            if !quiet {
                println!(
                    "distill: {} sessions, {} calls, {} observations, {} summaries, {} waiting for more work, {:.1}s",
                    s.sessions,
                    s.calls,
                    s.observations,
                    s.summaries,
                    s.skipped_small,
                    t.elapsed().as_secs_f64()
                );
            }
            for e in &s.errors {
                if quiet {
                    hook::log(&format!("distill: {e}"));
                } else {
                    eprintln!("distill error: {e}");
                }
            }
        }
        Cmd::Install {
            dry_run,
            only,
            bin,
            watch,
        } => {
            let has = |a: &str| only.split(',').any(|x| x.trim() == a);
            install::run(&install::Plan {
                bin: bin.unwrap_or_else(install::default_bin),
                dry_run,
                claude: has("claude"),
                codex: has("codex"),
                pi: has("pi"),
                watch,
            })?;
        }
        Cmd::Uninstall { dry_run } => install::uninstall(dry_run)?,
        Cmd::Delta { session, cwd } => {
            hook::catch_up_recent(&mut conn, std::time::Duration::from_millis(200))?;
            if let Some(project) = hook::project_for(&conn, Some(&session), cwd.as_deref())
                && let Some(d) = hook::cross_agent_delta(&conn, &session, &project)?
            {
                println!("{d}");
            }
        }
        Cmd::Tool { name, args } => {
            let a: serde_json::Value = serde_json::from_str(&args)?;
            println!("{}", mcp::call(&conn, &name, &a)?);
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
