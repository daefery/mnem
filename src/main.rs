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
        /// Also serve the web viewer and embedding service on this port (0 = off;
        /// default: config ui_port, else 37777)
        #[arg(long)]
        ui_port: Option<u16>,
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
        /// Also use the settings (config.json) the backup carries; the current file is
        /// kept as config.json.bak-<time>
        #[arg(long)]
        settings: bool,
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
        /// keyword, vector, fill or hybrid (default)
        #[arg(long, default_value = "hybrid")]
        mode: String,
        /// Print the cosine distribution of true targets vs best wrong candidates
        #[arg(long)]
        cosines: bool,
        /// Test set in ~/.mnem/eval: recall (model-written), vague (hand-written),
        /// real-dev or real-test (real prompts, see --build-real)
        #[arg(long, default_value = "recall")]
        set: String,
        /// Sample N real prompts from transcripts into real-dev and real-test
        #[arg(long)]
        build_real: Option<usize>,
        /// Have the distillation models judge what recall shows for real prompts
        #[arg(long)]
        judge: bool,
        /// Judge with this one model instead (and report agreement with the default judge)
        #[arg(long)]
        judge_model: Option<String>,
        /// Write each real prompt's top ten candidates with cosine and judgment (JSONL)
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Compare --dump files from different models (AUC, bootstrap CI, thresholds)
        #[arg(long, num_args = 1..)]
        analyze: Vec<PathBuf>,
        /// Also score each dumped candidate with this reranker (fastembed enum name)
        #[arg(long)]
        rerank: Option<String>,
        /// Release gate: run the installed mnem and this build on the same data and
        /// fail (exit 1) if recall got worse
        #[arg(long)]
        gate: bool,
        /// The baseline build for --gate (default: the mnem on PATH)
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Gate a settings change: the candidate runs with this config file
        #[arg(long)]
        candidate_config: Option<PathBuf>,
        /// Skip the judged real-prompt checks (the gate then fails; for a quick look)
        #[arg(long)]
        no_judge: bool,
        /// Print this build's gate metrics as JSON (used by --gate)
        #[arg(long, hide = true)]
        gate_metrics: bool,
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
        /// Keep vectors of other models (to compare models with `mnem eval`)
        #[arg(long)]
        keep: bool,
        /// First copy this model's vectors from another mnem database where the memory
        /// text still matches (the rest are embedded as usual)
        #[arg(long)]
        import: Option<PathBuf>,
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
        Cmd::Embed {
            probe,
            limit,
            keep,
            import,
        } => {
            let name = mnem::embed::model_name();
            let t = Instant::now();
            mnem::embed::fetch(&name)?;
            let t_load = Instant::now();
            let e = mnem::embed::Embedder::load()?;
            println!(
                "model {name}: ready in {:.1}s, loads in {} ms; vectors keyed {}",
                t.elapsed().as_secs_f64(),
                t_load.elapsed().as_millis(),
                e.key
            );
            if let Some(p) = probe {
                let t = Instant::now();
                let v = e.embed(&[p]);
                println!(
                    "query embedding: {} dims in {} µs",
                    v[0].len(),
                    t.elapsed().as_micros()
                );
            }
            if let Some(from) = import {
                let (copied, skipped) = mnem::embed::import(
                    &mut conn,
                    &e.key,
                    e.embed(&["dimension".to_string()])[0].len(),
                    &from,
                )?;
                println!(
                    "imported {copied} vectors ({skipped} skipped: text changed or memory gone)"
                );
            }
            let t = Instant::now();
            let n = mnem::embed::backfill(&mut conn, &e, limit)?;
            println!("embedded {n} memories in {:.1}s", t.elapsed().as_secs_f64());
            let pruned = if keep {
                0
            } else {
                mnem::embed::prune(&conn, &e)?
            };
            if pruned > 0 {
                println!("removed {pruned} vectors of other models or revisions");
            }
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
        Cmd::Eval {
            build,
            mode,
            cosines,
            set,
            build_real,
            judge,
            judge_model,
            dump,
            analyze,
            rerank,
            gate,
            baseline,
            candidate_config,
            no_judge,
            gate_metrics,
        } => {
            if gate_metrics {
                let m = mnem::gate::metrics(&conn, !no_judge)?;
                println!("{}", serde_json::to_string(&m)?);
                return Ok(());
            }
            if gate {
                drop(conn);
                return run_gate(&path, baseline, candidate_config, !no_judge);
            }
            if !analyze.is_empty() {
                print!("{}", mnem::eval::analyze(&analyze, "cos")?);
                return Ok(());
            }
            if let Some(n) = build_real {
                let (dev, test) = mnem::eval::build_real(&conn, n)?;
                println!("sampled {dev} prompts into real-dev and {test} into real-test");
                return Ok(());
            }
            let path = mnem::eval::set_path(&set);
            if cosines {
                let (t, w) = mnem::eval::cosines(&conn, &path)?;
                let pct = |v: &[f32], p: f64| {
                    v.get(((v.len() as f64 - 1.0) * p).round() as usize)
                        .copied()
                        .unwrap_or(0.0)
                };
                for (name, v) in [("true target", &t), ("best wrong", &w)] {
                    println!(
                        "{name:>11}: n={} p10 {:.2} p25 {:.2} p50 {:.2} p75 {:.2} p90 {:.2}",
                        v.len(),
                        pct(v, 0.1),
                        pct(v, 0.25),
                        pct(v, 0.5),
                        pct(v, 0.75),
                        pct(v, 0.9)
                    );
                }
                return Ok(());
            }
            let mode = match mode.as_str() {
                "keyword" => mnem::recall::Mode::Keyword,
                "vector" => mnem::recall::Mode::Vector,
                "fill" => mnem::recall::Mode::Fill,
                _ => mnem::recall::Mode::Hybrid,
            };
            if let Some(n) = build {
                let written = mnem::eval::build(&conn, n, &path)?;
                println!("built {written} questions in {}", path.display());
            }
            let judge_with = judge_model.as_deref().or(judge.then_some("chain"));
            let reranker = rerank
                .as_deref()
                .map(mnem::rerank::Reranker::load)
                .transpose()?;
            let r = mnem::eval::run(
                &conn,
                &path,
                mode,
                judge_with,
                dump.as_deref(),
                reranker.as_ref(),
            )?;
            if !r.rerank_ms.is_empty() {
                let p = |q: f64| r.rerank_ms[((r.rerank_ms.len() - 1) as f64 * q) as usize];
                println!(
                    "rerank of each prompt's candidates: p50 {:.0} ms, p95 {:.0} ms",
                    p(0.5),
                    p(0.95)
                );
            }
            if r.judged.prompts > 0 {
                let j = &r.judged;
                println!(
                    "real prompts: {} · memories shown {:.1} per prompt",
                    j.prompts,
                    j.shown as f64 / j.prompts as f64
                );
                if let Some(name) = judge_with {
                    let (lo, hi) = mnem::eval::wilson(j.right, j.judged_shown);
                    let (plo, phi) = mnem::eval::wilson(j.helped, j.judged_prompts);
                    println!(
                        "judged ({name}): {} of {} memories shown helped ({:.0}%, 95% CI {:.0}-{:.0}%) · {} did not",
                        j.right,
                        j.judged_shown,
                        100.0 * j.right as f64 / j.judged_shown.max(1) as f64,
                        100.0 * lo,
                        100.0 * hi,
                        j.judged_shown - j.right,
                    );
                    println!(
                        "prompts with a useful memory: {} of {} judged ({:.0}%, 95% CI {:.0}-{:.0}%){}",
                        j.helped,
                        j.judged_prompts,
                        100.0 * j.helped as f64 / j.judged_prompts.max(1) as f64,
                        100.0 * plo,
                        100.0 * phi,
                        if j.unjudged > 0 {
                            format!(" · {} not judged (judge failed), left out", j.unjudged)
                        } else {
                            String::new()
                        }
                    );
                    if name != "chain" {
                        let (n, agreed, kappa) = mnem::eval::agreement(
                            &mnem::eval::judge_identity("chain")?,
                            &mnem::eval::judge_identity(name)?,
                        );
                        println!(
                            "agreement with the default judge: {agreed} of {n} shared memories, kappa {kappa:.2}"
                        );
                    }
                } else {
                    println!("add --judge to have the distillation models judge them");
                }
                if r.cases == 0 && r.negatives == 0 {
                    return Ok(());
                }
            }
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
            println!(
                "top-5 precision {:.0}% (share of recalled memories that are targets) · recalled nothing for {} answerable",
                100.0 * r.precision5,
                r.silent
            );
            if r.negatives > 0 {
                println!(
                    "no-answer prompts: {} · recalled something for {}",
                    r.negatives,
                    r.false_alarms.len()
                );
            }
            for (id, q) in r.misses.iter().take(8) {
                println!("  miss #{id}: {q}");
            }
            for q in r.false_alarms.iter().take(8) {
                println!("  false alarm: {q}");
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
        Cmd::Restore {
            snapshot,
            apply,
            settings,
        } => {
            if apply {
                let (_, origin) = backup::check_import(&snapshot)?;
                if settings
                    && let Some(r) = backup::review_settings(&snapshot)?
                    && !r.valid
                {
                    anyhow::bail!(
                        "the backup's settings are not valid ({}); nothing was changed. Retry without --settings",
                        r.error.unwrap_or_default()
                    );
                }
                let m = backup::restore(&snapshot, &mut conn, &backup::dir())?;
                if origin
                    .host
                    .as_deref()
                    .is_some_and(|h| h != backup::hostname())
                {
                    let n = backup::mark_foreign_sources(&conn)?;
                    if n > 0 {
                        println!("{n} transcripts from the other machine are kept as history");
                    }
                }
                println!(
                    "restored {} ({} memories, {} events)",
                    m.file, m.memories, m.events
                );
                if settings {
                    if backup::apply_settings(&snapshot)? {
                        println!("settings restored; restart mnem-watch to use them");
                    } else {
                        println!("the backup carries no settings; kept the current ones");
                    }
                }
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
            ui::serve(path, port, || {})?;
        }
        Cmd::Watch {
            interval,
            distill_every,
            ui_port,
        } => {
            let ui_port = ui_port.unwrap_or_else(mnem::embed::configured_port);
            // Hooks learn where the service listens only after the bind succeeds; until
            // then (or if it fails) they see no service and use keywords alone.
            if let Err(e) = mnem::embed::record_service_port(&conn, 0) {
                hook::log(&format!("watch port: {e:#}"));
            }
            if ui_port != 0 {
                let ui_path = path.clone();
                std::thread::spawn(move || {
                    let record = || {
                        if let Err(e) = db::open(&ui_path)
                            .and_then(|c| mnem::embed::record_service_port(&c, ui_port))
                        {
                            hook::log(&format!("watch port: {e:#}"));
                        }
                    };
                    if let Err(e) = ui::serve(ui_path.clone(), ui_port, record) {
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
                    match mnem::embed::backfill(&mut conn, &e, Some(500)) {
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

/// `mnem eval --gate`: baseline and candidate on the same data, then the verdict.
fn run_gate(
    live: &std::path::Path,
    baseline: Option<PathBuf>,
    candidate_config: Option<PathBuf>,
    judge: bool,
) -> Result<()> {
    let candidate = std::env::current_exe()?;
    let baseline = match baseline {
        Some(b) => b,
        None => std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join("mnem"))
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow::anyhow!("no mnem on PATH to compare with; pass --baseline"))?,
    };
    let same = std::fs::canonicalize(&baseline).ok() == std::fs::canonicalize(&candidate).ok();
    if same && candidate_config.is_none() {
        anyhow::bail!(
            "the candidate is the installed mnem itself; run the build you want to ship (for example target/release/mnem eval --gate), or pass --candidate-config"
        );
    }
    if let Some(c) = &candidate_config {
        let text = std::fs::read_to_string(c)?;
        serde_json::from_str::<mnem::config::Config>(&text)
            .map_err(|e| anyhow::anyhow!("{} is not a valid mnem config: {e}", c.display()))?;
    }
    println!("baseline:  {}", baseline.display());
    println!(
        "candidate: {}{}",
        candidate.display(),
        candidate_config
            .as_ref()
            .map(|c| format!(" with {}", c.display()))
            .unwrap_or_default()
    );
    let t = Instant::now();
    let snap = mnem::gate::Snapshot::take(live)?;
    let b = mnem::gate::metrics_of(&baseline, &snap.0, None, judge)?;
    let c = mnem::gate::metrics_of(&candidate, &snap.0, candidate_config.as_deref(), judge)?;
    drop(snap);
    println!(
        "measured in {:.0}s ({} → {})\n",
        t.elapsed().as_secs_f64(),
        b.model,
        c.model
    );
    let checks = mnem::gate::compare(&b, &c);
    let width = checks.iter().map(|k| k.name.len()).max().unwrap_or(0);
    for k in &checks {
        println!(
            "{} {:<width$}  {:>14} → {:<14} (needs {})",
            if k.pass { "PASS" } else { "FAIL" },
            k.name,
            k.baseline,
            k.candidate,
            k.limit
        );
    }
    let failed = checks.iter().filter(|k| !k.pass).count();
    if failed > 0 {
        println!(
            "\nrecall gate: FAILED ({failed} of {} checks); do not ship this change",
            checks.len()
        );
        std::process::exit(1);
    }
    println!("\nrecall gate: passed ({} checks)", checks.len());
    Ok(())
}
