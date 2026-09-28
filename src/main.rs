use anyhow::Result;
use clap::{Parser, Subcommand};
use mnem::model::Agent;
use mnem::{
    backup, context, db, distill, doctor, hook, import, ingest, install, mcp, project, search, ui,
};
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
        /// The user's prompt, for prompt-time recall
        #[arg(long)]
        prompt: Option<String>,
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
        /// Also serve the web viewer on this port (0 = off)
        #[arg(long, default_value_t = 37777)]
        ui_port: u16,
    },
    /// Web viewer for memory (local only): http://127.0.0.1:37777
    Ui {
        #[arg(long, default_value_t = 37777)]
        port: u16,
    },
    /// Show the distillation model chain as CLIProxyAPI serves it now, with cooldowns
    Models,
    /// Take a verified snapshot of the database now (kept: newest 7)
    Backup {
        #[arg(long, default_value_t = backup::KEEP)]
        keep: usize,
    },
    /// List snapshots with their row counts
    Backups,
    /// Check a snapshot restores cleanly; with --apply, replace the live database with it
    Restore {
        snapshot: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    /// Record the git working tree for a session (the pi extension calls this per turn)
    Snapshot {
        #[arg(long)]
        session: String,
        #[arg(long)]
        cwd: PathBuf,
    },
    /// Delete memories (123), events (E123), a session or a project, for good: re-import
    /// and transcript replay will not bring them back
    Forget {
        ids: Vec<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Pin a fact every agent sees at session start (this project, or --global)
    Remember {
        fact: String,
        #[arg(long)]
        global: bool,
        /// Project id (default: resolved from the current directory)
        #[arg(long)]
        project: Option<String>,
    },
    /// Export sessions, events, memories and evidence as JSONL (stdout or --out)
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Measure prompt recall on a known-item test set (built with --build N)
    Eval {
        /// Build a new test set of N questions with the distillation models first
        #[arg(long)]
        build: Option<usize>,
        /// keyword, vector or hybrid (default)
        #[arg(long, default_value = "hybrid")]
        mode: String,
    },
    /// List pinned facts (forget one with `mnem forget <id>`)
    Pins,
    /// Download the embedding model (once) and embed memories that have no vector yet
    Embed {
        /// Also time a query embedding for this text
        #[arg(long)]
        probe: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
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
        Cmd::Hook { .. } | Cmd::Delta { .. } | Cmd::Context { .. } | Cmd::Snapshot { .. }
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
        Cmd::Embed { probe, limit } => {
            let name = mnem::embed::model_name();
            let t = Instant::now();
            mnem::embed::fetch(&name)?;
            let t_load = Instant::now();
            let e = mnem::embed::Embedder::load()?;
            println!(
                "model {name}: ready in {:.1}s, loads in {} ms",
                t.elapsed().as_secs_f64(),
                t_load.elapsed().as_millis()
            );
            if let Some(p) = probe {
                let t = Instant::now();
                let v = e.embed(&[p]);
                println!("query embedding: {} dims in {} µs", v[0].len(), t.elapsed().as_micros());
            }
            let t = Instant::now();
            let n = mnem::embed::backfill(&mut conn, &e, limit)?;
            println!("embedded {n} memories in {:.1}s", t.elapsed().as_secs_f64());
        }
        Cmd::Pins => {
            let mut st = conn.prepare(
                "SELECT id, project, coalesce(narrative, title) FROM memories WHERE kind = 'pinned' ORDER BY project, created_at",
            )?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for r in rows {
                let (id, project, fact) = r?;
                let scope = if project == "*" {
                    "every project".to_string()
                } else {
                    project
                };
                println!("#{id}  [{scope}]  {fact}");
            }
        }
        Cmd::Export { out, project } => {
            let n = match &out {
                Some(p) => {
                    let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
                    mnem::eval::export(&conn, &mut f, project.as_deref())?
                }
                None => {
                    mnem::eval::export(&conn, &mut std::io::stdout().lock(), project.as_deref())?
                }
            };
            eprintln!("export: {n} records");
        }
        Cmd::Eval { build, mode } => {
            let mode = match mode.as_str() {
                "keyword" => mnem::recall::Mode::Keyword,
                "vector" => mnem::recall::Mode::Vector,
                _ => mnem::recall::Mode::Hybrid,
            };
            let path = mnem::eval::eval_path();
            if let Some(n) = build {
                let written = mnem::eval::build(&conn, n, &path)?;
                println!("built {written} questions in {}", path.display());
            }
            let r = mnem::eval::run(&conn, &path, mode)?;
            println!(
                "recall eval: {} cases ({} sensitive skipped) · hit@1 {:.0}% · hit@5 {:.0}% · MRR {:.2} · p50 {:.1} ms · p95 {:.1} ms",
                r.cases,
                r.skipped,
                100.0 * r.hit1 as f64 / r.cases.max(1) as f64,
                100.0 * r.hit5 as f64 / r.cases.max(1) as f64,
                r.mrr,
                r.p50_ms,
                r.p95_ms
            );
            for (id, q) in r.misses.iter().take(8) {
                println!("  miss #{id}: {q}");
            }
        }
        Cmd::Forget {
            ids,
            session,
            project,
        } => {
            if ids.is_empty() && session.is_none() && project.is_none() {
                anyhow::bail!("nothing to forget: pass ids, --session or --project");
            }
            let f = mnem::forget::forget(&mut conn, &ids, session.as_deref(), project.as_deref())?;
            println!(
                "forgot {} memories, {} events, {} sessions (tombstoned: they will not come back)",
                f.memories, f.events, f.sessions
            );
        }
        Cmd::Remember {
            fact,
            global,
            project,
        } => {
            let project = if global {
                None
            } else {
                let cwd = std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                Some(
                    project
                        .or_else(|| hook::project_for(&conn, None, cwd.as_deref()))
                        .ok_or_else(|| {
                            anyhow::anyhow!("cannot resolve a project; pass --project or --global")
                        })?,
                )
            };
            let id = mnem::forget::remember(&conn, &fact, project.as_deref())?;
            println!(
                "pinned #{id} for {}",
                project.as_deref().unwrap_or("every project")
            );
        }
        Cmd::Snapshot { session, cwd } => {
            let recorded = mnem::gitstate::record(&conn, &session, &cwd)?;
            println!("{}", if recorded { "recorded" } else { "unchanged" });
        }
        Cmd::Backup { keep } => {
            let t = Instant::now();
            let m = backup::create(&conn, &backup::dir(), keep)?;
            println!(
                "backup: {} ({:.0} MB, {} sessions, {} events, {} memories, integrity ok) in {:.1}s",
                m.file,
                m.bytes as f64 / 1e6,
                m.sessions,
                m.events,
                m.memories,
                t.elapsed().as_secs_f64()
            );
        }
        Cmd::Backups => {
            for (p, m) in backup::list(&backup::dir())? {
                match m {
                    Some(m) => println!(
                        "{}  {:.0} MB  {} ago  {} memories  {} events",
                        m.file,
                        m.bytes as f64 / 1e6,
                        context::ago(db::now_ms() - m.created_at),
                        m.memories,
                        m.events
                    ),
                    None => println!("{}  (no manifest: unverified)", p.display()),
                }
            }
        }
        Cmd::Restore { snapshot, apply } => {
            if apply {
                let m = backup::restore(&snapshot, &mut conn, &backup::dir())?;
                println!(
                    "restored {} ({} memories, {} events)",
                    m.file, m.memories, m.events
                );
            } else {
                let m = backup::verify(&snapshot)?;
                println!(
                    "verified {}: integrity ok, checksum and counts match, search works ({} memories, {} events). Use --apply to restore.",
                    m.file, m.memories, m.events
                );
            }
            return Ok(());
        }
        Cmd::Models => {
            let llm = distill::Llm::from_config()?;
            llm.load_cooldowns(&conn);
            for (i, (model, note)) in llm.describe().into_iter().enumerate() {
                println!("{:>2}. {model}{note}", i + 1);
            }
        }
        Cmd::Ui { port } => {
            drop(conn);
            ui::serve(path, port)?;
        }
        Cmd::Watch {
            interval,
            distill_every,
            ui_port,
        } => {
            if ui_port != 0 {
                let ui_path = path.clone();
                std::thread::spawn(move || {
                    if let Err(e) = ui::serve(ui_path, ui_port) {
                        hook::log(&format!("watch ui: {e:#}"));
                    }
                });
            }
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
                // New memories (distilled, imported, pinned) get vectors on the next pass.
                if let Some(e) = mnem::embed::shared() {
                    match mnem::embed::backfill(&mut conn, e, Some(500)) {
                        Ok(n) if n > 0 => hook::log(&format!("watch embed: {n} memories")),
                        Ok(_) => {}
                        Err(e) => hook::log(&format!("watch embed: {e:#}")),
                    }
                }
                // Costly health checks happen here, not in hooks.
                if let Err(e) =
                    mnem::health::record_watch_report(&conn, mnem::health::stuck_files(&conn))
                {
                    hook::log(&format!("watch health: {e:#}"));
                }
                if backup::newest_age(&backup::dir()).is_none_or(|age| age > backup::INTERVAL_MS) {
                    match backup::create(&conn, &backup::dir(), backup::KEEP) {
                        Ok(m) => hook::log(&format!(
                            "watch backup: {} ({} memories)",
                            m.file, m.memories
                        )),
                        Err(e) => hook::log(&format!("watch backup FAILED: {e:#}")),
                    }
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
                    "distill: {} sessions, {} calls, {} observations, {} summaries, {} waiting for more work, models {:?}, {:.1}s",
                    s.sessions,
                    s.calls,
                    s.observations,
                    s.summaries,
                    s.skipped_small,
                    s.models,
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
        Cmd::Delta {
            session,
            cwd,
            prompt,
        } => {
            hook::catch_up_recent(&mut conn, std::time::Duration::from_millis(200))?;
            if let Some(project) = hook::project_for(&conn, Some(&session), cwd.as_deref())
                && let Some(update) =
                    hook::prompt_update(&conn, &session, &project, prompt.as_deref())
            {
                println!("{update}");
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
