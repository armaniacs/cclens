//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::config::{
    claude_config_dir, read_agent_surfaces, read_claude_md_surfaces, read_mcp_server_surfaces,
    read_project_surfaces, read_rule_surfaces, read_skill_surfaces,
};
use crate::adapter::config_file;
use crate::adapter::harness::Harness;
use crate::adapter::kilocode as kilocode_mod;
use crate::adapter::opencode as opencode_mod;
use crate::adapter::transcript::{
    count_permission_denials, extract_prompt_pointers, extract_tool_errors, extract_work_events,
    parse_session, parse_subagent_sidecar, session_cwd, subagent_id_from_file_name,
    subagent_prompt_id, subagent_sidecar_path, subagents_dir,
};
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::friction::ErrorCategory;
use crate::core::optimize as optimize_mod;
use crate::core::scope::{ScopeFilter, split_friction};
use crate::core::span::{DEFAULT_IDLE_GAP_MS, SessionStart, extract_spans, session_start};
use crate::core::subagent::{SplitCoverage, SubagentRun, link_runs};
use crate::core::surface::{
    LoadMode, Scope, StartupSavings, Surface, Wedge, classify_wedge, is_usage_measurable,
    on_demand_tokens, startup_savings,
};
use crate::core::thrash::detect_thrash;
use crate::core::usage::{attribute_subagents, extract_usage_events, output_tokens};
use crate::store::{SessionMeta, Store};

#[derive(Parser)]
#[command(
    name = "saniwalens",
    // Cargo.toml holds a placeholder that the release workflow and the flake
    // both stamp from the tag before building; build.rs additionally stamps
    // the git tag itself, so `--version` names the release even for a plain
    // source build.
    version = env!("SANIWALENS_VERSION"),
    about = "See where your coding-agent time and tokens go, and which config to fix.\n\
             Start with `saniwalens doctor`; every report explains itself and names the\n\
             follow-up command."
)]
pub struct Cli {
    /// Which agent harness to analyze. Global so every command resolves its
    /// store and input roots through it; the default keeps every existing
    /// invocation byte-identical in behavior. See `docs/specs/harness.md`.
    #[arg(long, global = true)]
    harness: Option<String>,
    /// Named preset from the config file to run under. Global so every
    /// command resolves through it; an explicit --harness or --db still wins
    /// over whatever the profile sets. See `docs/specs/cli.md`.
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read your session logs and config into the local store. Runs
    /// automatically before every report — call it directly only to refresh
    /// without reading anything.
    Analyze {
        /// Transcript root (default: $CLAUDE_CONFIG_DIR/projects, or
        /// ~/.claude/projects).
        #[arg(long)]
        projects: Option<PathBuf>,
        /// Output format: table | json (the run's counters).
        #[arg(long)]
        format: Option<String>,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// How often each skill ran and what it cost — per skill, or over time
    /// with --by.
    Usage {
        /// Bucket usage by time: year | month | week | day | hour (JST).
        #[arg(long)]
        by: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Everything installed in your config (skills, rules, agents, MCP
    /// servers, CLAUDE.md), what it weighs, and whether it was ever used.
    Inventory {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Cleanup opportunities: config that is never used, loaded every session
    /// while heavy, or rarely worth its cost — ranked by what removing it
    /// actually saves.
    Waste {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// What every session pays in context before any work starts, and how
    /// much of that comes from your own config files vs the harness itself.
    Overhead {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// How you steer Claude: the mix of instructing, steering ("go ahead"),
    /// correcting ("no, instead…"), and questioning prompts — and what that
    /// mix suggests changing.
    Prompts {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Tool calls that kept failing the same way — recurring failures that
    /// waste turns and tokens, each with a suggested fix.
    Failures {
        /// Show one config layer only: global (failures spread across
        /// projects — habits), project (each project's own failures), or
        /// project:<slug> (everything in that one project).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Files Claude kept re-editing in rapid bursts — where it got stuck
    /// retrying instead of making progress.
    Stuck {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// The health check — what to fix first, what your sessions cost, and
    /// what is fine as-is. Start here.
    Doctor {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table (human text) | json (the routed findings).
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Ask anything the fixed reports don't cover: run a read-only SQL query
    /// against the analyzed store (as an argument, or piped via stdin).
    /// `SELECT sql FROM sqlite_master` shows the schema.
    Sql {
        /// The SQL to run. If omitted, the query is read from stdin.
        query: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Store path (default: $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Act on the findings: opens an advisor session pre-loaded
    /// with the full analysis, which investigates each problem and proposes
    /// concrete config fixes (interactively under Claude Code, headless
    /// under opencode).
    Optimize {
        /// Transcript root (default: $CLAUDE_CONFIG_DIR/projects, or
        /// ~/.claude/projects).
        #[arg(long)]
        projects: Option<PathBuf>,
        /// Store to analyze into / read from (default:
        /// $XDG_STATE_HOME/saniwalens/saniwalens.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Optimize one config layer only: global (~/.claude) | project |
        /// project:<slug>.
        #[arg(long)]
        scope: Option<String>,
        /// Use the existing store as-is; skip the analyze step.
        #[arg(long)]
        frozen: bool,
        /// Print the composed prompt instead of launching the advisor.
        #[arg(long)]
        print: bool,
        /// Stream the advisor's raw output instead of the quiet render
        /// (headless opencode runs log raw output to a file by default).
        #[arg(long)]
        verbose: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // The tool's own config file is optional; without it everything below is
    // exactly the old behavior. Flags beat the file everywhere.
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let config_path = config_file_path(config_home.as_deref(), home_dir());
    let file = config_path
        .as_deref()
        .map(config_file::load)
        .transpose()?
        .flatten();
    let config_dir = config_path.as_deref().and_then(|p| p.parent());
    let resolved = config_file::resolve(
        cli.harness.as_deref(),
        cli.profile.as_deref(),
        None,
        file.as_ref(),
        config_dir,
    )?;
    let harness = parse_harness(resolved.harness.as_deref())?;
    let config_db = resolved.db;
    match cli.command {
        Command::Analyze {
            projects,
            format,
            db,
        } => analyze(
            &harness,
            projects,
            parse_format(format.as_deref())?,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Usage {
            by,
            format,
            frozen,
            db,
        } => usage(
            &harness,
            by.as_deref(),
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Inventory {
            scope,
            format,
            frozen,
            db,
        } => inventory(
            &harness,
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Waste {
            scope,
            format,
            frozen,
            db,
        } => waste(
            &harness,
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Overhead { format, frozen, db } => overhead(
            &harness,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Prompts { format, frozen, db } => prompts(
            &harness,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Failures {
            scope,
            format,
            frozen,
            db,
        } => failures(
            &harness,
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Stuck { format, frozen, db } => stuck(
            &harness,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Doctor {
            scope,
            format,
            frozen,
            db,
        } => doctor(
            &harness,
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Sql { query, format, db } => sql(
            &harness,
            query.as_deref(),
            parse_format(format.as_deref())?,
            &resolve_db(db, &harness, config_db.as_deref())?,
        ),
        Command::Optimize {
            projects,
            db,
            scope,
            frozen,
            print,
            verbose,
        } => optimize(
            &harness,
            projects,
            &resolve_db(db, &harness, config_db.as_deref())?,
            &parse_scope(scope.as_deref())?,
            frozen,
            print,
            verbose,
        ),
    }
}

/// Parse `--harness`. Absent means Claude Code — every pre-flag invocation
/// keeps its behavior, which is what makes the flag upstream-mergeable.
fn parse_harness(value: Option<&str>) -> Result<Harness> {
    match value {
        None => Ok(Harness::Claude),
        Some(v) => Harness::parse(v)
            .with_context(|| format!("unknown --harness '{v}' (claude | opencode | kilocode)")),
    }
}

fn parse_scope(value: Option<&str>) -> Result<ScopeFilter> {
    match value {
        None => Ok(ScopeFilter::All),
        Some(v) => ScopeFilter::parse(v)
            .with_context(|| format!("unknown --scope '{v}' (global | project | project:<slug>)")),
    }
}

/// Analyze, compose the advisor prompt from the findings, and hand it to an
/// advisor session — the AI-proposal layer's entry point. The pure
/// composition lives in `core::optimize`; this shell does the I/O: run the
/// analysis, read the store into `Findings`, then launch the harness's agent.
/// See `docs/specs/harness.md` for the per-harness launch shape.
/// Launch the harness's advisor for an optimize session: the binary plus any
/// leading argv. `claude` opens interactively on the prompt; `opencode run`
/// investigates headlessly — JSON events (`--format json`) so the raw tool
/// dumps can be logged while only the agent's text streams, and a title so
/// the sessions it seeds are recognizable in the store it feeds.
fn launch_command(
    harness: &Harness,
) -> Option<(&'static str, &'static [&'static str], optimize_mod::RunMode)> {
    match harness {
        Harness::Claude => Some(("claude", &[], optimize_mod::RunMode::Interactive)),
        Harness::Opencode => Some((
            "opencode",
            &["run", "--format", "json", "--title", "saniwalens optimize"],
            optimize_mod::RunMode::Headless,
        )),
        // The `kilo` binary is the measured agent CLI (Homebrew), and kilo is
        // an OpenCode fork whose TUI takes a prompt: `kilo [project] --prompt
        // <prompt>` opens the interactive session on the pointer prompt — the
        // experience the claude launch gives, with the user present to approve
        // the briefing read and steer (kilo gates every tool call where
        // opencode does not, so a headless run would need `--auto` instead).
        // A bare `kilo <text>` positional is treated as a file to open
        // (measured), not a prompt — `--prompt` is the TUI's message surface.
        Harness::Kilocode => Some(("kilo", &["--prompt"], optimize_mod::RunMode::Interactive)),
    }
}
///
/// The briefing carries concrete paths and error excerpts that may be sensitive,
/// so it is written to a private (`0600`) temp file and only a pointer is passed
/// on argv — never the data, which `ps` would otherwise expose. The file is
/// removed as soon as the session ends. `--print` instead writes the full prompt
/// (briefing inline) to stdout for piping / inspection.
fn optimize(
    harness: &Harness,
    projects: Option<PathBuf>,
    db: &Path,
    filter: &ScopeFilter,
    frozen: bool,
    print: bool,
    verbose: bool,
) -> Result<()> {
    if !frozen {
        let stats = run_analyze(harness, projects, db)?;
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "store: refreshed just now ({} {} re-analyzed, {} unchanged)",
                stats.sessions,
                harness.input_unit(),
                stats.skipped
            ))
        );
    }
    let store = Store::open(db).context("open store")?;
    // `--frozen` skips the analyze that guards, so guard the launch path here.
    guard_harness(&store, harness, db)?;
    let findings = collect_findings(&store)?;

    // The absolute store path so the session's `saniwalens sql --db …` hits the
    // store this run built, wherever it is launched from.
    let db_display = std::fs::canonicalize(db)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| db.to_string_lossy().into_owned());

    // The harness copy for every prompt and briefing rendered here — plain
    // data assembled from the harness, so the core stays harness-agnostic.
    let copy = optimize_mod::HarnessCopy {
        agent: harness.display_name(),
        article: harness.article(),
        source: harness.source_noun(),
        raw_source: harness.raw_source(),
        global_ref: harness.global_config_ref(),
        project_ref: harness.project_config_ref(),
        instruction_file: harness.instruction_file(),
        config_inventory: harness.config_inventory(),
    };

    if print {
        println!(
            "{}",
            optimize_mod::compose_prompt(&findings, &db_display, filter, &copy)
        );
        return Ok(());
    }

    // The measured launch surfaces are Claude Code and opencode. kilo's
    // prompt-launch surface is unmeasured — a positional argument is treated
    // as a file to open (measured), not a prompt — so the launch is refused
    // and `--print` is the path that hands the briefing over.
    let Some((binary, leading, mode)) = launch_command(harness) else {
        anyhow::bail!(
            "the {} launch is not supported yet — use --print to get the briefing for pasting into kilo",
            harness.display_name()
        );
    };

    // Sensitive briefing → private temp file; only a data-free pointer on argv.
    let briefing = optimize_mod::render_briefing(&findings, filter, &copy);
    let briefing_path = write_private_tempfile(&briefing).context("write briefing temp file")?;
    let prompt = optimize_mod::launch_prompt(
        &briefing_path.to_string_lossy(),
        &db_display,
        filter,
        &copy,
        mode,
    );

    // Hand over the terminal. Headless harnesses run piped: the raw stream is
    // logged to a private file and only the agent's text (narration, then the
    // final plan) streams on stdout — the tool dumps stay in the log, and
    // `--verbose` keeps the old raw streaming. `claude` opens an interactive
    // TUI on the pointer prompt (stdio inherited, the user takes over). Clean
    // up the briefing file the moment the run ends, whatever the outcome.
    let launch_note = || {
        format!(
            "launch `{} {}` — is {} installed and on PATH?",
            binary,
            leading.join(" ").trim_end(),
            harness.display_name()
        )
    };
    let (result, run_log) = if mode == optimize_mod::RunMode::Headless && !verbose {
        let (status, log) = run_headless(binary, leading, &prompt).with_context(launch_note)?;
        (Ok(status), Some(log))
    } else {
        let status = std::process::Command::new(binary)
            .args(leading)
            .arg(&prompt)
            .status();
        (status, None)
    };
    let _ = fs::remove_file(&briefing_path);

    let status = result.with_context(launch_note)?;
    if !status.success() {
        match run_log {
            Some(log) => anyhow::bail!(
                "{binary} exited with {status} — raw output: {}",
                log.display()
            ),
            None => anyhow::bail!("{binary} exited with {status}"),
        }
    }
    Ok(())
}

/// Run the headless advisor with its raw output logged: spawn with stdout
/// piped, write every raw line (and the child's stderr) to a private log, and
/// surface only what the agent says — text parts on stdout (so a pipe like
/// `| pbcopy` receives the narration and the plan), one-line progress markers
/// on stderr. A heartbeat covers quiet stretches (a long tool call, model
/// thinking) so the run never looks hung. Returns the log path so the caller
/// can point at the full detail. The raw event shapes are unversioned
/// upstream, so rendering is lenient: a line with no recognizable text stays
/// in the log, and a run that renders nothing readable says so instead of
/// leaving silence.
fn run_headless(
    binary: &str,
    leading: &[&str],
    prompt: &str,
) -> Result<(std::process::ExitStatus, PathBuf)> {
    use std::io::{BufRead, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    let log_path = private_temp_path("saniwalens-run", "log");
    write_private_file(&log_path, "").context("create run log")?;
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .context("open run log")?;
    let child_err = fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .context("open run log for the advisor")?;

    eprintln!(
        "{}",
        Style::stderr().dim(&format!(
            "advisor running headlessly — raw progress: {}",
            log_path.display()
        ))
    );

    // A dim beat every 30s while the advisor works: elapsed seconds and how
    // many events have streamed, so silence reads as waiting, not a hang.
    let events = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let heartbeat = {
        let events = Arc::clone(&events);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut next = 30u64;
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let elapsed = start.elapsed().as_secs();
                if elapsed >= next {
                    next += 30;
                    eprintln!(
                        "{}",
                        Style::stderr().dim(&format!(
                            "  … advisor working ({}s, {} events)",
                            elapsed,
                            events.load(Ordering::Relaxed)
                        ))
                    );
                }
            }
        })
    };

    let mut launch = std::process::Command::new(binary);
    launch.args(leading).arg(prompt);
    launch
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(child_err));
    let mut child = launch.spawn()?;
    let stdout = child.stdout.take().context("pipe the advisor's stdout")?;

    let mut readable = false;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.context("read the advisor's output")?;
        events.fetch_add(1, Ordering::Relaxed);
        writeln!(log, "{line}").context("write the run log")?;
        match opencode_mod::render_run_line(&line) {
            opencode_mod::RunEvent::Text(text) => {
                readable = true;
                println!("{text}");
            }
            opencode_mod::RunEvent::Progress(tool) => {
                eprintln!("{}", Style::stderr().dim(&format!("  → {tool}")));
            }
            opencode_mod::RunEvent::Silent => {}
        }
    }
    let status = child.wait()?;
    stop.store(true, Ordering::Relaxed);
    heartbeat.join().ok();
    if status.success() && !readable {
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "the advisor produced no readable text — full output: {}",
                log_path.display()
            ))
        );
    }
    Ok((status, log_path))
}

/// A fresh private temp path for `prefix`, unique per process and call.
fn private_temp_path(prefix: &str, ext: &str) -> PathBuf {
    // The clock is here to avoid a collision with a leftover file, not to be
    // unguessable — exclusive creation is what makes guessing the name useless.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos:x}.{ext}", std::process::id()))
}

/// Write `contents` to a fresh file in the temp dir. Used for the optimization
/// briefing, which may hold sensitive paths/excerpts and must not sit on argv or
/// be world-readable.
fn write_private_tempfile(contents: &str) -> Result<PathBuf> {
    let path = private_temp_path("saniwalens-briefing", "md");
    write_private_file(&path, contents)?;
    Ok(path)
}

/// Create `path` and write `contents` to it, for the current user only.
///
/// The temp dir is shared on unix, so the file has to be **created
/// exclusively**: a same-user process that pre-creates the path — as a symlink
/// to somewhere it can read — would otherwise capture the briefing, and no
/// permission bits set afterwards would help. Losing that race now fails the
/// open instead of following what is already there.
///
/// `0600` narrows it further on unix. Windows has no mode to set here; the
/// default per-user `%LOCALAPPDATA%\Temp` is what keeps other unprivileged
/// users out, so the guarantee there rests on the directory rather than the
/// file. Setting an explicit ACL would mean a new dependency for a second lock
/// on a door that is already locked.
fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents.as_bytes())?;
    Ok(())
}

/// Gather the COMPLETE analysis the optimization briefing carries — every view's
/// detail as owned data for `core::optimize`, so the seeded session works from
/// the briefing and need not re-run the tool. Lists are capped where a long tail
/// adds nothing for ranking.
fn collect_findings(store: &Store) -> Result<optimize_mod::Findings> {
    // The briefing's "unknown floor" sentinel is 0 — no session start observed
    // means the always-on section is omitted rather than guessed at.
    let floor = store.baseline_floor()?.unwrap_or(0);

    // A few concrete examples per (project, category) — the actual failing
    // paths/files behind each count, so the fix is obvious from the briefing.
    const EXAMPLES_PER_CATEGORY: u32 = 3;
    let mut examples: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for (project, category, excerpt) in store.error_examples(EXAMPLES_PER_CATEGORY)? {
        examples
            .entry((project, category))
            .or_default()
            .push(excerpt);
    }

    // The split of each (project, category) across the tools that produced it —
    // so the briefing carries the attribution the agent otherwise re-derives.
    let mut by_tool: std::collections::HashMap<(String, String), Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for (project, category, tool, n) in store.error_tool_breakdown()? {
        by_tool
            .entry((project, category))
            .or_default()
            .push((tool, n));
    }

    // Route actionable friction to the layer that owns each fix (core::scope):
    // majority-owned categories to their project, the spread rest to global.
    let cells: Vec<(String, String, i64)> = store
        .error_counts_by_project()?
        .into_iter()
        .filter(|(_, label, _)| ErrorCategory::from_label(label).is_actionable())
        .collect();
    let split = split_friction(&cells);

    // A global category aggregates its tool split across projects and pools a
    // few examples; a project-owned one uses its own (project, category) detail.
    const EXAMPLES_PER_GLOBAL_CATEGORY: usize = 3;
    let friction_global: Vec<optimize_mod::FrictionCat> = split
        .global
        .iter()
        .map(|g| {
            let mut tools: std::collections::HashMap<String, i64> =
                std::collections::HashMap::new();
            let mut pooled = Vec::new();
            for (project, label, _) in &cells {
                if label != &g.category {
                    continue;
                }
                let key = (project.clone(), label.clone());
                for (tool, n) in by_tool.get(&key).into_iter().flatten() {
                    *tools.entry(tool.clone()).or_default() += n;
                }
                pooled.extend(examples.get(&key).into_iter().flatten().cloned());
            }
            let mut by_tool: Vec<(String, i64)> = tools.into_iter().collect();
            by_tool.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            pooled.truncate(EXAMPLES_PER_GLOBAL_CATEGORY);
            optimize_mod::FrictionCat {
                label: g.category.clone(),
                count: g.total,
                projects: g.projects,
                by_tool,
                examples: pooled,
            }
        })
        .collect();

    // Workflow: Bash mix + cd share, most-edited files (global signals).
    let bash = store.work_counts("bash_cmd")?;
    let bash_total: i64 = bash.iter().map(|(_, n)| n).sum();
    let cd_pct = (bash_total > 0).then(|| {
        let cd = bash.iter().find(|(c, _)| c == "cd").map_or(0, |(_, n)| *n);
        (cd as f64 * 100.0 / bash_total as f64).round() as i64
    });
    let top_commands: Vec<(String, i64)> = bash.into_iter().take(15).collect();
    let hotspots: Vec<(String, i64)> = store
        .work_counts("file_edit")?
        .into_iter()
        .take(15)
        .collect();

    // Thrash per project — a burst belongs to the project it happened in. Each
    // episode already knows its project, so detection runs once over everything.
    let mut thrash_by_project: std::collections::HashMap<String, Vec<optimize_mod::ThrashLine>> =
        std::collections::HashMap::new();
    for episode in detect_thrash(&store.file_edits()?, 5 * 60, 4) {
        let lines = thrash_by_project
            .entry(episode.project.clone())
            .or_default();
        if lines.len() < 5 {
            lines.push(optimize_mod::ThrashLine {
                span_secs: episode.span_secs(),
                file: episode.file().to_string(),
                edits: episode.edits,
            });
        }
    }

    // Config wedges, split by the surface's own scope.
    let scoped = config_wedges(store)?;

    // Assemble per-project findings: any project owning friction, thrash, or
    // project-scoped wedges gets a section, busiest first.
    let roots: std::collections::HashMap<String, String> = store
        .session_roots()?
        .into_iter()
        .map(|(root, project)| (project, root))
        .collect();
    let mut owned_friction: std::collections::HashMap<String, Vec<(String, i64)>> =
        split.per_project.into_iter().collect();
    let mut project_names: Vec<String> = owned_friction
        .keys()
        .chain(thrash_by_project.keys())
        .chain(scoped.projects.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut projects: Vec<optimize_mod::ProjectFindings> = project_names
        .drain(..)
        .map(|project| {
            let friction = owned_friction
                .remove(&project)
                .unwrap_or_default()
                .into_iter()
                .map(|(label, count)| {
                    let key = (project.clone(), label.clone());
                    optimize_mod::FrictionCat {
                        by_tool: by_tool.get(&key).cloned().unwrap_or_default(),
                        examples: examples.get(&key).cloned().unwrap_or_default(),
                        label,
                        count,
                        projects: 1,
                    }
                })
                .collect();
            let wedges = scoped.projects.get(&project);
            optimize_mod::ProjectFindings {
                friction,
                thrash: thrash_by_project.remove(&project).unwrap_or_default(),
                unused: wedges.map(|w| w.unused.clone()).unwrap_or_default(),
                always_on_heavy: wedges
                    .map(|w| w.always_on_heavy.clone())
                    .unwrap_or_default(),
                root: roots.get(&project).cloned().unwrap_or_default(),
                project,
            }
        })
        .collect();
    // Busiest first: owned friction, then config wedges, then thrash — so a
    // friction-free but config-heavy project still outranks thrash-only noise.
    projects.sort_by(|a, b| {
        let weight = |p: &optimize_mod::ProjectFindings| {
            (
                p.total_friction(),
                p.unused.len() + p.always_on_heavy.len(),
                p.thrash.len(),
            )
        };
        weight(b)
            .cmp(&weight(a))
            .then_with(|| a.project.cmp(&b.project))
    });

    let pcounts = store.prompt_behavior_counts()?;
    let ptotal: i64 = pcounts.iter().map(|(_, n)| n).sum();
    let share = |name: &str| {
        let n = pcounts
            .iter()
            .find(|(b, _)| b == name)
            .map_or(0, |(_, n)| *n);
        (n as f64 * 100.0 / ptotal as f64).round() as i64
    };
    let (steer_pct, correct_pct) = if ptotal > 0 {
        (Some(share("steer")), Some(share("correct")))
    } else {
        (None, None)
    };

    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    let agents: Vec<optimize_mod::AgentCost> = store
        .subagent_by_agent()?
        .into_iter()
        .map(|row| optimize_mod::AgentCost {
            agent: row.agent,
            runs: row.runs,
            out_tokens: row.out_tokens,
        })
        .collect();
    Ok(optimize_mod::Findings {
        main_out: store.skill_usage()?.iter().map(|r| r.out_tokens).sum(),
        sub_tokens,
        sub_agents,
        agent_split: SplitCoverage::new(
            sub_tokens,
            sub_agents,
            agents.iter().map(|a| (a.runs, a.out_tokens)),
        ),
        agents,
        floor,
        config_tokens: if floor > 0 {
            store.always_on_config_tokens()?
        } else {
            0
        },
        friction_global,
        cd_pct,
        top_commands,
        hotspots,
        unused: scoped.global.unused,
        always_on_heavy: scoped.global.always_on_heavy,
        steer_pct,
        correct_pct,
        projects,
    })
}

/// Wedge-flagged surfaces (unused-but-measurable, always-on heavy) grouped by
/// the config layer that owns them — the routing key for summary and briefing.
#[derive(Default)]
struct WedgeRefs {
    unused: Vec<optimize_mod::SurfaceRef>,
    always_on_heavy: Vec<optimize_mod::SurfaceRef>,
}

struct ScopedWedges {
    global: WedgeRefs,
    projects: std::collections::HashMap<String, WedgeRefs>,
}

fn config_wedges(store: &Store) -> Result<ScopedWedges> {
    // Carry the cost split, not one figure: a skill's file weight is its
    // invocation cost, and rendering it as the trim saving promised startup
    // context that deleting it never frees (`docs/specs/surfaces.md`).
    let surface_ref =
        |e: &crate::store::CatalogEntry, load_mode: LoadMode| optimize_mod::SurfaceRef {
            kind: e.kind.clone(),
            id: e.id.clone(),
            startup_tokens: e.startup_tokens,
            on_demand_tokens: on_demand_tokens(load_mode, e.static_tokens.map(|t| t as u64))
                .map(|t| t as i64),
        };
    let mut scoped = ScopedWedges {
        global: WedgeRefs::default(),
        projects: std::collections::HashMap::new(),
    };
    for entry in store.effective_catalog()? {
        let refs = if entry.scope == "global" {
            &mut scoped.global
        } else {
            scoped.projects.entry(entry.project.clone()).or_default()
        };
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|t| t as u64);
        if is_usage_measurable(&entry.kind) && entry.uses == 0 {
            refs.unused.push(surface_ref(&entry, load_mode));
        }
        if load_mode.is_always_on() && static_tokens.is_some_and(|t| t >= HEAVY_TOKENS) {
            refs.always_on_heavy.push(surface_ref(&entry, load_mode));
        }
    }
    Ok(scoped)
}

/// Run a read-only SQL query against the store and print the result. The query
/// comes from the argument or, when absent, from stdin — so both
/// `saniwalens sql '…'` and `echo '…' | saniwalens sql` work. The store is
/// opened read-only so an ad-hoc query can never mutate the derived data.
fn sql(harness: &Harness, query: Option<&str>, format: Format, db: &Path) -> Result<()> {
    let query = match query {
        Some(q) => q.to_string(),
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read SQL from stdin")?;
            buf
        }
    };
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("no SQL provided — pass it as an argument or on stdin");
    }

    let store = Store::open_readonly(db)
        .with_context(|| format!("open store {} (run `analyze` first?)", db.display()))?;
    // A store built by another harness is a wrong-harness read even here —
    // `sql` cannot migrate or refresh, but it can still refuse loudly.
    guard_harness(&store, harness, db)?;
    // `sql` never mutates the store, so it cannot auto-refresh — surface the
    // store's age instead so a stale read is at least visible. An old db whose
    // schema predates `meta` just skips the header.
    if let Ok(analyzed_at) = store.meta("analyzed_at") {
        eprint_freshness(&freshness_line(analyzed_at.as_deref()));
    }
    if format == Format::Json {
        // Typed rows as objects — SQLite integers/reals/NULLs survive as JSON
        // numbers/null. Duplicate column names keep the last value.
        let (columns, rows) = store.query_json(query)?;
        let objects: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|row| serde_json::Value::Object(columns.iter().cloned().zip(row).collect()))
            .collect();
        return emit_json(&serde_json::Value::Array(objects));
    }
    let (columns, rows) = store.query(query)?;
    let headers: Vec<&str> = columns.iter().map(String::as_str).collect();
    let aligns = vec![Align::Left; columns.len()];
    render(&headers, &aligns, &rows, format);
    note(&format!("{} row(s)", rows.len()));
    Ok(())
}

/// One-screen health check: the few most actionable findings from every view,
/// split by the config layer that owns each fix — the global picture first,
/// then each project's own findings ("optimize my global setup" and "optimize
/// this project" are different tasks). `--scope` narrows to one layer.
fn doctor(
    harness: &Harness,
    filter: &ScopeFilter,
    format: Format,
    frozen: bool,
    db: &Path,
) -> Result<()> {
    if format == Format::Markdown {
        anyhow::bail!("summary has no markdown form — use the default text or --format json");
    }
    let store = open_for_read(harness, db, frozen)?;
    let f = collect_findings(&store)?;

    if format == Format::Json {
        // The routed findings, filtered the same way the text form is: the
        // global picture (minus the projects list) and each visible project.
        let global = filter.includes_global().then(|| {
            let mut value = serde_json::to_value(&f).expect("findings serialize");
            value.as_object_mut().expect("object").remove("projects");
            value
        });
        let projects: Vec<&optimize_mod::ProjectFindings> = f
            .projects
            .iter()
            .filter(|p| filter.includes_project(&p.project))
            .collect();
        return emit_json(&serde_json::json!({
            "global": global,
            "projects": projects,
        }));
    }

    // The text form is written for someone who has never seen saniwalens's
    // vocabulary: every item says what happened, why it costs, and what to do
    // — prioritized, so the report reads top to bottom as a to-do list.
    let style = Style::stdout();
    let (session_count, project_count) = store.session_stats()?;
    if session_count == 0 {
        println!("nothing analyzed yet — run `saniwalens analyze` first");
        return Ok(());
    }
    println!(
        "{}",
        style.heading(&format!(
            "{} usage health check — {session_count} sessions across \
             {project_count} projects",
            harness.display_name()
        ))
    );

    let visible: Vec<_> = f
        .projects
        .iter()
        .filter(|p| filter.includes_project(&p.project))
        .collect();
    let display = |p: &optimize_mod::ProjectFindings| {
        if p.root.is_empty() {
            p.project.clone()
        } else {
            tilde_path(&p.root)
        }
    };

    // ---- WHAT TO FIX FIRST: recurring problems, biggest first. -------------
    struct Item {
        weight: i64,
        lines: Vec<String>,
    }
    let mut items: Vec<Item> = Vec::new();
    for p in &visible {
        if p.total_friction() == 0 {
            continue;
        }
        let mut lines = vec![format!(
            "In {}, {} tool calls failed in recurring ways:",
            style.bold(&display(p)),
            style.warn(&p.total_friction().to_string())
        )];
        for cat in p.friction.iter().take(3) {
            lines.push(format!(
                "  {:>4}× {} {}",
                cat.count,
                cat.label,
                style.dim(&format!(
                    "— {}",
                    ErrorCategory::from_label(&cat.label).suggestion(harness.instruction_file())
                ))
            ));
        }
        if let Some(worst) = p.thrash.first() {
            lines.push(format!(
                "  …and the agent got stuck re-editing {} ({} edits in {}m).",
                worst.file,
                worst.edits,
                worst.span_secs.div_euclid(60).max(1)
            ));
        }
        lines.push(format!(
            "  Fix in this project's own {} — details: {}",
            harness.project_config_ref(),
            style.command(&format!(
                "saniwalens failures --scope project:{}",
                p.project
            ))
        ));
        items.push(Item {
            weight: p.total_friction(),
            lines,
        });
    }
    if filter.includes_global() {
        for cat in &f.friction_global {
            items.push(Item {
                weight: cat.count,
                lines: vec![
                    format!(
                        "The same failure hit {} different projects ({}× total): {} {}",
                        cat.projects,
                        style.warn(&cat.count.to_string()),
                        style.bold(&cat.label),
                        style.dim(&format!(
                            "— {}",
                            ErrorCategory::from_label(&cat.label)
                                .suggestion(harness.instruction_file())
                        ))
                    ),
                    format!(
                        "  No single project owns it, so the fix belongs in {} — \
                         details: {}",
                        harness.global_config_ref(),
                        style.command("saniwalens failures --scope global")
                    ),
                ],
            });
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.weight));
    let dropped_items = items.len().saturating_sub(4);
    items.truncate(4);
    if filter.includes_global() {
        if let Some(cd_pct) = f.cd_pct.filter(|pct| *pct >= 25) {
            items.push(Item {
                weight: 0,
                lines: vec![format!(
                    "{} of all Bash commands are just `cd` — a working-directory \
                     convention in your global {} would cut the churn.",
                    style.warn(&format!("{cd_pct}%")),
                    harness.instruction_file()
                )],
            });
        }
        if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct)
            && (correct >= 10 || steer >= 25)
        {
            items.push(Item {
                weight: 0,
                lines: vec![format!(
                    "{steer}% of your prompts steer and {correct}% correct — rework that \
                     clearer upfront specs could cut. Details: {}",
                    style.command("saniwalens prompts")
                )],
            });
        }
    }

    println!("\n{}", style.heading("WHAT TO FIX FIRST"));
    if items.is_empty() {
        println!("  {}", style.good("nothing recurring found — looking good"));
    }
    for (i, item) in items.iter().enumerate() {
        let (first, rest) = item.lines.split_first().expect("item has lines");
        println!("  {}. {first}", i + 1);
        for line in rest {
            println!("   {line}");
        }
    }
    // Never truncate silently (reporting honesty, cli.md).
    if dropped_items > 0 {
        println!(
            "  {}",
            style.dim(&format!(
                "… and {dropped_items} smaller recurring problem(s) — see `saniwalens failures --scope project`"
            ))
        );
    }

    // ---- COST: what a session pays, in plain words. -------------------------
    if filter.includes_global() {
        println!("\n{}", style.heading("COST (token estimates, not billing)"));
        if f.floor > 0 {
            let residual = (f.floor - f.config_tokens).max(0);
            println!(
                "  Every session starts with ~{} tokens of context before any work:",
                style.bold(&fmt_tokens(f.floor))
            );
            if residual > 0 {
                println!(
                    "  {} comes from your own global config files; the other {} is {}",
                    fmt_tokens(f.config_tokens),
                    fmt_tokens(residual),
                    harness.display_name()
                );
                println!(
                    "  {}",
                    style.dim(
                        "itself (system prompt, built-in tools, MCP schemas) plus each \
                         project's own config (AGENTS.md, skill descriptions) — `saniwalens \
                         overhead` splits it per project."
                    )
                );
            } else {
                // The global config estimate already covers the observed floor —
                // attributing a "0" to the harness would read as a claim.
                println!(
                    "  ~all of it is accounted for by your own config files (skill \
                     descriptions, AGENTS.md) — nothing is left over to attribute; \
                     `saniwalens overhead` splits it per project."
                );
            }
        }
        println!(
            "  Output written so far: {} tokens by skills on the main thread, {} by {} subagent runs.",
            fmt_tokens(f.main_out),
            fmt_tokens(f.sub_tokens),
            f.sub_agents
        );
        warn_missing_agent_split(&f.agent_split);
        for line in doctor_agent_lines(&f, &style.command("saniwalens usage")) {
            if line.starts_with('…') {
                println!("  {}", style.dim(&line));
            } else {
                println!("  {line}");
            }
        }
    }

    // ---- CONFIG WORTH PRUNING: installed-but-dead weight, by owner. ---------
    let mut pruning: Vec<String> = Vec::new();
    if filter.includes_global() && (!f.unused.is_empty() || !f.always_on_heavy.is_empty()) {
        pruning.push(format!(
            "  {}: {} → {}",
            harness.global_config_ref(),
            prune_phrase(&f.unused, &f.always_on_heavy),
            style.command("saniwalens waste --scope global")
        ));
    }
    let mut prunable: Vec<_> = visible
        .iter()
        .filter(|p| !p.unused.is_empty() || !p.always_on_heavy.is_empty())
        .collect();
    prunable.sort_by_key(|p| std::cmp::Reverse(p.unused.len() + p.always_on_heavy.len()));
    for p in prunable.iter().take(3) {
        pruning.push(format!(
            "  {}: {} → {}",
            display(p),
            prune_phrase(&p.unused, &p.always_on_heavy),
            style.command(&format!("saniwalens waste --scope project:{}", p.project))
        ));
    }
    if !pruning.is_empty() {
        println!("\n{}", style.heading("CONFIG WORTH PRUNING"));
        println!(
            "  {}",
            style.dim("Installed config that costs attention (or context) without earning it:")
        );
        for line in pruning {
            println!("{line}");
        }
        if prunable.len() > 3 {
            println!(
                "  {}",
                style.dim(&format!(
                    "… and {} more project(s) with prunable config — see `saniwalens waste --scope project`",
                    prunable.len() - 3
                ))
            );
        }
    }

    // ---- LOOKS HEALTHY: say explicitly what needs no action. ----------------
    if filter.includes_global() {
        let mut healthy: Vec<String> = Vec::new();
        if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct)
            && correct < 10
            && steer < 25
        {
            healthy.push(format!(
                "prompting mix ({steer}% steering / {correct}% corrections) — good autonomy balance"
            ));
        }
        if f.friction_global.is_empty() {
            healthy.push("no failure habit spans multiple projects".to_string());
        }
        if f.always_on_heavy.is_empty() && f.floor > 0 {
            healthy.push(format!(
                "your global config is lean ({} of the ~{} startup tokens)",
                fmt_tokens(f.config_tokens),
                fmt_tokens(f.floor)
            ));
        }
        if !healthy.is_empty() {
            println!("\n{}", style.heading("LOOKS HEALTHY"));
            for line in healthy {
                println!("  {}", style.good(&line));
            }
        }
    }
    Ok(())
}

/// `"3 unused surfaces, 4 always-on files costing ~8.5k tokens/session"` — the
/// pruning line's plain-language phrase, zero parts dropped.
fn prune_phrase(
    unused: &[optimize_mod::SurfaceRef],
    always_on_heavy: &[optimize_mod::SurfaceRef],
) -> String {
    let mut parts = Vec::new();
    if !unused.is_empty() {
        parts.push(format!(
            "{} installed but never used",
            plural(unused.len(), "surface")
        ));
    }
    if !always_on_heavy.is_empty() {
        let tokens: i64 = always_on_heavy
            .iter()
            .filter_map(|s| s.startup_tokens)
            .sum();
        parts.push(format!(
            "{} loaded every session (~{} tokens each time)",
            plural(always_on_heavy.len(), "heavy file"),
            fmt_tokens(tokens)
        ));
    }
    parts.join(", ")
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The doctor's lines naming the agents behind the subagent figure — "which
/// agent" is the actionable half of it. The top rows are introduced as "most
/// of" the output only when they actually exceed half of it; when the rows
/// are a partial split (`SplitCoverage`), the gap is stated first so the
/// names are not read as the whole bill. `full_split` is the styled command
/// that shows every row. An unavailable split yields nothing: that case is
/// reported on stderr with its refresh hint.
fn doctor_agent_lines(f: &optimize_mod::Findings, full_split: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let top: Vec<String> = f
        .agents
        .iter()
        .take(3)
        .map(|a| {
            format!(
                "{} {} ({} run{})",
                a.label(),
                fmt_tokens(a.out_tokens),
                a.runs,
                if a.runs == 1 { "" } else { "s" }
            )
        })
        .collect();
    if top.is_empty() {
        return lines;
    }
    let coverage = &f.agent_split;
    let shown: i64 = f.agents.iter().take(top.len()).map(|a| a.out_tokens).sum();
    let intro = if coverage.is_partial() {
        lines.push(format!(
            "Only {} of those tokens ({} of {} runs) have a per-agent split — the rest were \
             analyzed before runs were extracted and their transcripts are gone.",
            fmt_tokens(coverage.known_tokens),
            coverage.known_runs,
            coverage.total_runs
        ));
        "Among the runs that have one"
    } else if coverage.is_majority(shown) {
        "Most of that subagent output"
    } else {
        "The largest agent types"
    };
    lines.push(format!(
        "{intro}: {}. Full split: {full_split}",
        top.join(", ")
    ));
    // Never truncate silently (reporting honesty, cli.md).
    if f.agents.len() > top.len() {
        lines.push(format!(
            "… and {} more agent type(s) not shown",
            f.agents.len() - top.len()
        ));
    }
    lines
}

/// Humanize a token count: `6467406` → `6.5M`, `32139` → `32.1k`.
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// One dimmed framing block above a table: what this view shows and how to
/// read it, so the output explains itself. Table format only — markdown is for
/// pasting the table, json is the machine contract.
fn preamble(format: Format, text: &str) {
    if matches!(format, Format::Table) {
        println!("{}\n", Style::stdout().dim(text));
    }
}

/// Shorten an absolute path under the home directory to `~/…` for display.
fn tilde_path(path: &str) -> String {
    shorten_to_tilde(path, home_dir())
}

/// The shortening itself, separated from the environment lookup so it stays
/// pure and testable. A separator must follow the prefix, so `/home/metadata`
/// is not mistaken for something under `/home/me`; either separator counts, so
/// a Windows path shortens against a Windows home.
fn shorten_to_tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    // The two sides come from different places — `home` from the environment,
    // `path` from the cwd a transcript recorded — so on Windows they can spell
    // the same directory differently; the OS accepts either. Only the
    // comparison is folded. The suffix is sliced from the original at the same
    // offset, which stays valid because folding swaps one ASCII byte for
    // another, so the display keeps whatever spelling the path really had.
    let fold = |s: &str| s.replace('\\', "/");
    match fold(path).strip_prefix(&fold(home)) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// Thrash bursts: a file edited many times in a short window — where the agent
/// got stuck and kept retrying, as opposed to a hotspot's healthy spread-out
/// edits. An episode is one session's edits to one path, so parallel work on
/// a same-named file is never presented as one agent looping (`core::thrash`).
fn stuck(harness: &Harness, format: Format, frozen: bool, db: &Path) -> Result<()> {
    const GAP_SECS: i64 = 5 * 60;
    const MIN_EDITS: u32 = 4;
    let store = open_for_read(harness, db, frozen)?;
    let episodes = detect_thrash(&store.file_edits()?, GAP_SECS, MIN_EDITS);
    if format == Format::Json {
        let episodes: Vec<serde_json::Value> = episodes
            .iter()
            .map(|e| {
                serde_json::json!({
                    "project": e.project,
                    "session_id": e.session_id,
                    "file": e.file(),
                    "path": e.path,
                    "edits": e.edits,
                    "span_secs": e.span_secs(),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "episodes": episodes }));
    }
    if episodes.is_empty() {
        println!("no thrash episodes (>= {MIN_EDITS} rapid re-edits) — run analyze first");
        return Ok(());
    }
    preamble(
        format,
        "Files the agent kept re-editing in rapid bursts — the signature of getting stuck\n\
         and retrying. Worth checking what blocked it there (a rule? missing context?).",
    );
    let rows: Vec<Vec<String>> = episodes
        .iter()
        .take(20)
        .map(|e| {
            let span = e.span_secs();
            vec![
                e.project.clone(),
                e.file().to_string(),
                e.edits.to_string(),
                format!("{}m{}s", span / 60, span % 60),
            ]
        })
        .collect();
    render(
        &["project", "file", "edits", "within"],
        &[Align::Left, Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );
    note(&format!(
        "bursts of >= {MIN_EDITS} edits to one file within {}m by a single session —\n\
         likely where the agent got stuck. `--format json` carries the session id and\n\
         full path to investigate with.",
        GAP_SECS / 60
    ));
    Ok(())
}

/// A dimmed footnote under a table — context, not data.
fn note(text: &str) {
    println!("\n{}", Style::stdout().dim(text));
}

/// Where the work stumbles: recurring tool failures by category, ranked, each
/// with what it suggests fixing. This is about the work, not the config —
/// recurring failures are fixable friction that wastes turns and tokens.
/// `--scope` routes by ownership (`core::scope`): `global` shows the failures
/// no project owns, `project` each project's majority-owned failures, and
/// `project:<slug>` everything that happened in that one project.
fn failures(
    harness: &Harness,
    filter: &ScopeFilter,
    format: Format,
    frozen: bool,
    db: &Path,
) -> Result<()> {
    struct FrictionRow {
        project: Option<String>,
        category: String,
        count: i64,
        /// How many projects the count spreads across (global scope only).
        spread: Option<usize>,
    }

    let store = open_for_read(harness, db, frozen)?;
    let cells = store.error_counts_by_project()?;

    let counts: Vec<FrictionRow> = match filter {
        ScopeFilter::All => store
            .error_counts()?
            .into_iter()
            .map(|(category, count)| FrictionRow {
                project: None,
                category,
                count,
                spread: None,
            })
            .collect(),
        ScopeFilter::Global => split_friction(&cells)
            .global
            .into_iter()
            .map(|g| FrictionRow {
                project: None,
                category: g.category,
                count: g.total,
                spread: Some(g.projects),
            })
            .collect(),
        ScopeFilter::Project(None) => split_friction(&cells)
            .per_project
            .into_iter()
            .flat_map(|(project, cats)| {
                cats.into_iter().map(move |(category, count)| FrictionRow {
                    project: Some(project.clone()),
                    category,
                    count,
                    spread: None,
                })
            })
            .collect(),
        ScopeFilter::Project(Some(slug)) => cells
            .into_iter()
            .filter(|(project, _, _)| project == slug)
            .map(|(project, category, count)| FrictionRow {
                project: Some(project),
                category,
                count,
                spread: None,
            })
            .collect(),
    };
    let total: i64 = counts.iter().map(|row| row.count).sum();

    if format == Format::Json {
        let failures: Vec<serde_json::Value> = counts
            .iter()
            .map(|row| {
                serde_json::json!({
                    "project": row.project,
                    "category": row.category,
                    "count": row.count,
                    "spread_projects": row.spread,
                    "suggestion": ErrorCategory::from_label(&row.category)
                        .suggestion(harness.instruction_file()),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "total": total, "failures": failures }));
    }
    if total == 0 {
        println!("no tool failures found for this scope — run `saniwalens analyze` first?");
        return Ok(());
    }

    preamble(
        format,
        &match filter {
            ScopeFilter::All => {
                "Tool calls that kept failing the same way, grouped by kind of failure.\n\
                 Every row wastes turns and tokens; the suggestion says what usually fixes it."
                    .to_string()
            }
            ScopeFilter::Global => {
                format!(
                    "Failures spread across several projects — no single project owns them, so\n\
                     they read as habits or global-config problems. Fix these in {}.",
                    harness.global_config_ref()
                )
            }
            ScopeFilter::Project(None) => {
                format!(
                    "Failures each project owns (it holds the majority of that kind). Fix each\n\
                     in that project's own {}, not globally.",
                    harness.project_config_ref()
                )
            }
            ScopeFilter::Project(Some(_)) => {
                format!(
                    "Every recurring failure inside this one project, most frequent first.\n\
                     Fix them in this project's own {}.",
                    harness.project_config_ref()
                )
            }
        },
    );
    let with_project = matches!(filter, ScopeFilter::Project(None));
    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|row| {
            let mut cells = Vec::new();
            if with_project {
                cells.push(row.project.clone().unwrap_or_default());
            }
            let error = match row.spread {
                Some(spread) => format!("{} (across {spread} projects)", row.category),
                None => row.category.clone(),
            };
            cells.extend([
                error,
                row.count.to_string(),
                ErrorCategory::from_label(&row.category).suggestion(harness.instruction_file()),
            ]);
            cells
        })
        .collect();
    let (headers, aligns): (Vec<&str>, Vec<Align>) = if with_project {
        (
            vec!["project", "error", "count", "suggestion"],
            vec![Align::Left, Align::Left, Align::Right, Align::Left],
        )
    } else {
        (
            vec!["error", "count", "suggestion"],
            vec![Align::Left, Align::Right, Align::Left],
        )
    };
    render(&headers, &aligns, &rows, format);
    note(&format!(
        "{total} tool failures (denials from the transcript's own marker where present; \
         everything else is a lexical heuristic)"
    ));
    Ok(())
}

/// Show the mix of how the user steers the session, and what it suggests. Heavy
/// steering points to room for more autonomy; frequent corrections point to
/// clearer upfront specs. The classes are lexical heuristics (`core::prompt`).
fn prompts(harness: &Harness, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(harness, db, frozen)?;
    let counts = store.prompt_behavior_counts()?;
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    if format == Format::Json {
        let behaviors: Vec<serde_json::Value> = counts
            .iter()
            .map(|(behavior, n)| {
                serde_json::json!({
                    "behavior": behavior,
                    "count": n,
                    "share_pct": if total > 0 {
                        (*n as f64 * 100.0 / total as f64).round() as i64
                    } else {
                        0
                    },
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "total": total, "behaviors": behaviors }));
    }
    if total == 0 {
        // An analyzed store with sessions but no prompts means this harness
        // harvests no prompt signal (opencode v1 does not) — telling the user
        // to run `analyze` again would send them in a circle.
        let (sessions, _) = store.session_stats()?;
        if sessions > 0 {
            println!(
                "no prompts harvested for this harness — the store holds {sessions} analyzed session(s)"
            );
        } else {
            println!("no prompts found — run `saniwalens analyze` first");
        }
        return Ok(());
    }

    preamble(
        format,
        &format!(
            "How you steer {}, classified per prompt: instruct (substantive asks),\n\
             steer (\"go ahead\" / \"yes\"), correct (\"no, instead…\"), question. Lots of\n\
             steering = room for more autonomy; lots of correcting = unclear upfront specs.",
            harness.display_name()
        ),
    );
    let pct = |n: i64| (n as f64 * 100.0 / total as f64).round() as i64;
    let get = |name: &str| {
        counts
            .iter()
            .find(|(b, _)| b == name)
            .map_or(0, |(_, n)| *n)
    };

    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|(behavior, n)| vec![behavior.clone(), n.to_string(), format!("{}%", pct(*n))])
        .collect();
    render(
        &["behavior", "count", "share"],
        &[Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );

    let style = Style::stdout();
    note(&format!(
        "{total} prompts (behavioral classes are lexical heuristics)"
    ));
    let steer = pct(get("steer"));
    let correct = pct(get("correct"));
    let mut flagged = false;
    if steer >= 25 {
        flagged = true;
        println!(
            "  - {}",
            style.warn(&format!(
                "{steer}% steering (\"go ahead\" / \"yes\" / \"next\"): you approve in small \
                 steps — room for more autonomy (clearer upfront scope, /loop, longer leash)."
            ))
        );
    }
    if correct >= 10 {
        flagged = true;
        println!(
            "  - {}",
            style.warn(&format!(
                "{correct}% corrections (\"no\" / \"instead\" / \"戻して\"): rework after a \
                 wrong turn — tighter initial specs or rules could cut it."
            ))
        );
    }
    if !flagged {
        println!(
            "  - {}",
            style.good(&format!(
                "healthy mix: mostly substantive instructions, low correction ({correct}%) — \
                 good alignment, no obvious babysitting or rework problem."
            ))
        );
    }
    Ok(())
}

/// Reconcile the empirical always-on floor against readable config. The floor is
/// what every session actually starts with; subtracting the config we can read
/// leaves the residual — system prompt, built-in tools, and MCP tool schemas the
/// catalog cannot weigh. The per-project floors let you infer an MCP server's
/// real cost by comparing a project that enables it against one that does not.
fn overhead(harness: &Harness, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(harness, db, frozen)?;
    let floor = store.baseline_floor()?;
    let config = store.always_on_config_tokens()?;
    let residual = floor.map(|floor| (floor - config).max(0));
    let per_project = store.baseline_floor_per_project()?;

    if format == Format::Json {
        let per_project: Vec<serde_json::Value> = per_project
            .iter()
            .map(|(project, floor, sessions)| {
                serde_json::json!({
                    "project": project,
                    "floor": floor,
                    "sessions": sessions,
                })
            })
            .collect();
        return emit_json(&serde_json::json!({
            "floor": floor,
            "config_tokens": config,
            "residual": residual,
            "per_project": per_project,
        }));
    }
    // No session start observed — say so rather than substituting a mid-session
    // context that would read as a startup cost.
    let Some(floor) = floor else {
        let (sessions, _) = store.session_stats()?;
        if sessions == 0 {
            println!("no data — run `saniwalens analyze` first");
        } else {
            println!(
                "always-on floor unavailable — none of the {sessions} analyzed session(s)\n\
                 carries an observable session start. A store written before session-start\n\
                 tracking keeps its old rows (analyze never re-reads a closed session), so\n\
                 the floor appears as newer sessions are analyzed. Discarding the store\n\
                 would backfill it from the {} still on disk, at the cost of every\n\
                 session whose {} {} has already pruned.",
                harness.raw_source(),
                harness.source_noun(),
                harness.display_name()
            );
        }
        return Ok(());
    };
    let residual = (floor - config).max(0);

    preamble(
        format,
        "What every session pays in context before any work starts, and how much of\n\
         it your own config files account for.",
    );
    let style = Style::stdout();
    println!(
        "Observed always-on floor (leanest session-start context): {}",
        style.bold(&format!("{floor} tokens"))
    );
    println!(
        "  readable config ({}): {config} tokens",
        harness.readable_config()
    );
    println!(
        "  residual (system prompt + built-in tools + MCP schemas):                {residual} tokens"
    );
    println!(
        "{}",
        style.dim(
            "  -> the residual is what you cannot read from files; compare projects below\n\
             \x20    to see an MCP server's marginal cost (a project that enables it starts higher)."
        )
    );
    println!();

    let rows: Vec<Vec<String>> = per_project
        .into_iter()
        .map(|(project, floor, sessions)| vec![project, floor.to_string(), sessions.to_string()])
        .collect();
    render(
        &["project", "floor_tokens", "sessions"],
        &[Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );
    note(
        "each floor is the leanest start across that project's sessions — a project with\n\
         few sessions may not have caught a fresh one, so compare like counts.",
    );
    Ok(())
}

/// Static-token threshold above which an always-on or rarely-used surface counts
/// as "heavy" — a tuning knob, not a hard rule.
const HEAVY_TOKENS: u64 = 800;

/// Counters from one analyze run, for the command's summary line and the
/// freshness header on auto-refreshing reads.
struct AnalyzeStats {
    sessions: usize,
    skipped: usize,
    spans_total: usize,
    surface_count: usize,
    denials: usize,
}

fn analyze(harness: &Harness, projects: Option<PathBuf>, format: Format, db: &Path) -> Result<()> {
    let stats = run_analyze(harness, projects, db)?;
    let store = Store::open(db).context("open store")?;
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    if format == Format::Json {
        return emit_json(&serde_json::json!({
            "sessions": stats.sessions,
            "unchanged": stats.skipped,
            "skill_invocations": stats.spans_total,
            "surfaces": stats.surface_count,
            "subagent_tokens": sub_tokens,
            "subagents": sub_agents,
            "permission_denials": stats.denials,
            "db": db.display().to_string(),
        }));
    }
    println!(
        "analyzed {} session(s) ({} unchanged), {} skill invocation(s), \
         {} surface(s) catalogued, \
         {sub_tokens} subagent tokens across {sub_agents} subagent(s), \
         {} permission denial(s) -> {}",
        stats.sessions,
        stats.skipped,
        stats.spans_total,
        stats.surface_count,
        stats.denials,
        db.display()
    );
    Ok(())
}

/// Analyze raw input into the store, dispatching on the harness. The guard
/// runs first: a store built by another harness refuses the write rather than
/// mixing rows (`docs/specs/harness.md`).
fn run_analyze(harness: &Harness, projects: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    guard_harness(&Store::open(db).context("open store")?, harness, db)?;
    match harness {
        Harness::Claude => run_analyze_claude(projects, db),
        Harness::Opencode => run_analyze_opencode(projects, db),
        Harness::Kilocode => run_analyze_kilocode(projects, db),
    }
}

/// Refuse to analyze into a store another harness built. A store with no
/// `harness` key predates the flag and is adopted (the Claude default), never
/// refused.
fn guard_harness(store: &Store, harness: &Harness, db: &Path) -> Result<()> {
    if let Some(existing) = store.meta("harness")?
        && existing != harness.label()
    {
        anyhow::bail!(
            "the store at {} holds {existing} sessions — rerun with --harness {existing} (or pick another --db)",
            db.display()
        );
    }
    Ok(())
}

fn run_analyze_claude(projects: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    let projects = projects.map(Ok).unwrap_or_else(default_projects_dir)?;
    let mut store = Store::open(db).context("open store")?;

    // Rows produced by older analysis semantics are rebuilt once: the
    // fingerprint skip below would otherwise pin them forever, because the
    // transcripts they came from will never change again.
    let reuse_ingested = store.is_analyzer_current()?;

    let mut sessions = 0;
    let mut skipped = 0;
    let mut spans_total = 0;
    let mut denials = 0;
    for transcript in main_transcripts(&projects)? {
        let path_str = transcript.display().to_string();
        // Incremental skip: an unchanged (mtime, size) was already ingested.
        // The stat is taken before the read so a file that grows concurrently
        // is guaranteed a mismatch — and a re-ingest — next run
        // (`docs/specs/storage.md`).
        let fingerprint = file_fingerprint(&transcript);
        if reuse_ingested
            && let Some((mtime, size)) = fingerprint
            && store.is_ingested(&path_str, mtime, size)?
        {
            skipped += 1;
            continue;
        }
        let text = fs::read_to_string(&transcript)
            .with_context(|| format!("read {}", transcript.display()))?;
        denials += count_permission_denials(&text);
        let records = parse_session(&text);
        let mut spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);
        let usage = extract_usage_events(&records);
        let runs = subagent_runs(&transcript);
        attribute_subagents(&mut spans, &runs);
        let sub_tokens: i64 = runs.iter().map(|run| run.out_tokens as i64).sum();
        let root = session_cwd(&text).map(|cwd| normalize_root(&cwd));
        let meta = session_meta(
            &transcript,
            root.unwrap_or_default(),
            sub_tokens,
            runs.len() as i64,
            session_start(&records),
        );
        store.ingest_session(&meta, &spans, &usage)?;
        store.ingest_subagent_runs(&meta.id, &meta.source_path, &runs)?;
        let prompts: Vec<(usize, i64, &str)> = extract_prompt_pointers(&text)
            .into_iter()
            .map(|(line, ts, behavior)| (line, ts, behavior.label()))
            .collect();
        store.ingest_prompts(&meta.id, &meta.source_path, &prompts)?;
        // `raw_errors` owns the strings; `error_rows` borrows them.
        let raw_errors = extract_tool_errors(&text);
        let error_rows: Vec<(i64, &str, &str, &str, &str)> = raw_errors
            .iter()
            .map(|e| {
                (
                    e.epoch_ms,
                    e.category.label(),
                    e.excerpt.as_str(),
                    e.tool.as_str(),
                    e.target.as_str(),
                )
            })
            .collect();
        store.ingest_tool_errors(&meta.id, &meta.source_path, &error_rows)?;
        // `work` owns the strings; the rows borrow them.
        let work = extract_work_events(&text);
        let work_rows: Vec<(i64, &str, &str, Option<&str>)> = work
            .iter()
            .map(|e| (e.epoch_ms, e.kind, e.id.as_str(), e.path.as_deref()))
            .collect();
        store.ingest_work_events(&meta.id, &meta.source_path, &work_rows)?;
        if let Some((mtime, _)) = fingerprint {
            // Record the size actually consumed, so a tail appended after the
            // read shows a mismatch next run instead of being skipped forever.
            store.record_ingested_file(&path_str, mtime, text.len() as i64)?;
        }
        sessions += 1;
        spans_total += spans.len();
    }

    // Catalog the installed config — global scope plus every known project root
    // that still exists on disk — so usage joins against what is installed.
    let config_dir = claude_home()?;
    let mut surfaces = read_global_surfaces()?;
    for (root, project) in store.session_roots()? {
        if Path::new(&root).is_dir() {
            surfaces.extend(read_project_surfaces(
                Path::new(&root),
                &project,
                home_dir().map(Path::new),
            ));
        }
    }
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    store.record_analyzer_version()?;
    store.set_meta("analyzed_at", &chrono::Utc::now().to_rfc3339())?;
    store.set_meta("projects_dir", &projects.display().to_string())?;
    store.set_meta("config_dir", &config_dir.display().to_string())?;
    store.set_meta("harness", Harness::Claude.label())?;

    Ok(AnalyzeStats {
        sessions,
        skipped,
        spans_total,
        surface_count,
        denials,
    })
}

/// Analyze an opencode database into the opencode store. The row-to-record
/// mapping is `adapter::opencode`; this shell reads the database read-only,
/// groups rows by session, and ingests each session idempotently. See
/// `docs/specs/harness.md` for the v1 capability boundaries (no subagent
/// runs, no prompts, no permission denials, empty surface catalog).
fn run_analyze_opencode(input: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    let input = match input {
        Some(path) => path,
        None => default_opencode_db()?,
    };
    if !input.is_file() {
        anyhow::bail!(
            "no opencode database at {} — is opencode installed (or pass its path via --projects)?",
            input.display()
        );
    }
    let mut store = Store::open(db).context("open store")?;
    // The opencode version key lives apart from the shared analyzer version:
    // an opencode-only extraction change must never force Claude Code users
    // through a full re-analyze (`adapter::opencode::OPENCODE_ANALYZER_VERSION`).
    let reuse_ingested = store
        .meta(opencode_mod::OPENCODE_ANALYZER_META_KEY)?
        .as_deref()
        == Some(opencode_mod::OPENCODE_ANALYZER_VERSION);

    let snapshot = opencode_mod::read_database(&input)?;
    let mut messages_by_session: std::collections::HashMap<
        &str,
        Vec<&opencode_mod::SnapshotMessage>,
    > = std::collections::HashMap::new();
    for message in &snapshot.messages {
        messages_by_session
            .entry(message.session_id.as_str())
            .or_default()
            .push(message);
    }
    let mut parts_by_message: std::collections::HashMap<&str, Vec<&opencode_mod::SnapshotPart>> =
        std::collections::HashMap::new();
    for part in &snapshot.parts {
        parts_by_message
            .entry(part.message_id.as_str())
            .or_default()
            .push(part);
    }

    let mut sessions = 0;
    let mut skipped = 0;
    let mut spans_total = 0;
    for session in &snapshot.sessions {
        let session_messages = messages_by_session
            .get(session.id.as_str())
            .cloned()
            .unwrap_or_default();
        // Per-session fingerprint: the whole-file stat would re-ingest every
        // session on each run (opencode appends constantly).
        let fingerprint_key = format!("opencode:{}", session.id);
        let message_count = session_messages.len() as i64;
        if reuse_ingested
            && store.is_ingested(&fingerprint_key, session.time_updated_ms, message_count)?
        {
            skipped += 1;
            continue;
        }
        let message_rows: Vec<opencode_mod::MessageRow> = session_messages
            .iter()
            .map(|m| opencode_mod::MessageRow {
                id: m.id.as_str(),
                session_id: m.session_id.as_str(),
                time_created_ms: m.time_created_ms,
                parent_id: m.data.get("parentID").and_then(|v| v.as_str()),
                data: &m.data,
            })
            .collect();
        let mut part_rows: Vec<opencode_mod::PartRow> = session_messages
            .iter()
            .flat_map(|m| {
                parts_by_message
                    .get(m.id.as_str())
                    .cloned()
                    .unwrap_or_default()
            })
            .map(|p| opencode_mod::PartRow {
                id: p.id.as_str(),
                message_id: p.message_id.as_str(),
                session_id: p.session_id.as_str(),
                time_created_ms: p.time_created_ms,
                data: &p.data,
            })
            .collect();
        part_rows.sort_by_key(|p| (p.time_created_ms, p.id));
        let records = opencode_mod::parse_session(&message_rows, &part_rows);
        let spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);
        let usage = extract_usage_events(&records);
        let meta = SessionMeta {
            id: session.id.clone(),
            project: session.directory.clone(),
            slug: session.slug.clone(),
            root: session.directory.clone(),
            source_path: fingerprint_key.clone(),
            sub_tokens: 0,
            sub_agent_count: 0,
            start: session_start(&records),
        };
        store.ingest_session(&meta, &spans, &usage)?;
        // `raw_errors` owns the strings; `error_rows` borrows them.
        let raw_errors = opencode_mod::extract_tool_errors(&part_rows);
        let error_rows: Vec<(i64, &str, &str, &str, &str)> = raw_errors
            .iter()
            .map(|e| {
                (
                    e.epoch_ms,
                    e.category.label(),
                    e.excerpt.as_str(),
                    e.tool.as_str(),
                    e.target.as_str(),
                )
            })
            .collect();
        store.ingest_tool_errors(&meta.id, &meta.source_path, &error_rows)?;
        // `work` owns the strings; the rows borrow them.
        let work = opencode_mod::extract_work_events(&part_rows);
        let work_rows: Vec<(i64, &str, &str, Option<&str>)> = work
            .iter()
            .map(|e| (e.epoch_ms, e.kind, e.id.as_str(), e.path.as_deref()))
            .collect();
        store.ingest_work_events(&meta.id, &meta.source_path, &work_rows)?;
        store.record_ingested_file(&fingerprint_key, session.time_updated_ms, message_count)?;
        sessions += 1;
        spans_total += spans.len();
    }

    // Catalog the installed config — global scope plus every known project
    // root that still exists on disk — so usage joins against what is
    // installed. Skills join from the six documented sites (deduped by
    // canonical path, since sites may be symlinks to one shared directory);
    // MCP and permissions stay out of v1 (see
    // `docs/specs/config-format-opencode.md`), so their live usage still
    // shows as orphaned rows in `inventory`.
    let config_dir = opencode_home()?;
    let home = home_dir().map(Path::new);
    let mut seen = std::collections::HashSet::new();
    let mut surfaces = opencode_mod::read_global_surfaces(&config_dir, home, &mut seen);
    for (root, project) in store.session_roots()? {
        if Path::new(&root).is_dir() {
            surfaces.extend(opencode_mod::read_project_surfaces(
                Path::new(&root),
                &project,
                home,
                &mut seen,
            ));
        }
    }
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    store.set_meta(
        opencode_mod::OPENCODE_ANALYZER_META_KEY,
        opencode_mod::OPENCODE_ANALYZER_VERSION,
    )?;
    store.set_meta("analyzed_at", &chrono::Utc::now().to_rfc3339())?;
    store.set_meta("projects_dir", &input.display().to_string())?;
    store.set_meta("config_dir", &config_dir.display().to_string())?;
    store.set_meta("harness", Harness::Opencode.label())?;

    Ok(AnalyzeStats {
        sessions,
        skipped,
        spans_total,
        surface_count,
        denials: 0,
    })
}

/// The opencode config root for this run: the environment read that
/// `adapter::harness::opencode_config_dir` deliberately does not do.
fn opencode_home() -> Result<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Harness::opencode_config_dir(config_home.as_deref(), home_dir().map(Path::new))
        .context("set XDG_CONFIG_HOME or HOME")
}

/// The kilo config root for this run: the environment read that
/// `adapter::harness::kilocode_config_dir` deliberately does not do.
fn kilo_home() -> Result<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Harness::kilocode_config_dir(config_home.as_deref(), home_dir().map(Path::new))
        .context("set XDG_CONFIG_HOME or HOME")
}

/// The opencode database for this run: the environment read that
/// `adapter::harness::opencode_db_path` deliberately does not do.
fn default_opencode_db() -> Result<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    Harness::opencode_db_path(data_home.as_deref(), home_dir().map(Path::new))
        .context("set XDG_DATA_HOME or HOME")
}

/// Analyze kilocode task directories into the kilocode store. The file-to-
/// record mapping is `adapter::kilocode`; this shell reads each task's two
/// JSON files and ingests each task idempotently. See `docs/specs/harness.md`
/// for the v1 capability boundaries (no subagent runs, no prompts, no
/// permission denials, no surface catalog yet).
fn run_analyze_kilocode(input: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    let input = match input {
        Some(path) => path,
        None => default_kilocode_tasks()?,
    };
    if !input.is_dir() {
        anyhow::bail!(
            "no kilocode tasks directory at {} — is the kilocode extension installed (or pass its path via --projects)?",
            input.display()
        );
    }
    let mut store = Store::open(db).context("open store")?;
    // The kilocode version key lives apart from the shared analyzer version:
    // a kilocode-only extraction change must never force other harnesses
    // through a full re-analyze (`adapter::kilocode::KILOCODE_ANALYZER_VERSION`).
    let reuse_ingested = store
        .meta(kilocode_mod::KILOCODE_ANALYZER_META_KEY)?
        .as_deref()
        == Some(kilocode_mod::KILOCODE_ANALYZER_VERSION);

    let mut task_dirs: Vec<PathBuf> = std::fs::read_dir(&input)
        .with_context(|| format!("read {}", input.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    task_dirs.sort();

    let mut sessions = 0;
    let mut skipped = 0;
    let mut spans_total = 0;
    for task_dir in &task_dirs {
        let conversation_path = task_dir.join("api_conversation_history.json");
        let ui_path = task_dir.join("ui_messages.json");
        if !conversation_path.is_file() || !ui_path.is_file() {
            continue; // a task directory without both files is not a session
        }
        let task_id = task_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Per-task fingerprint: the directory stat would re-ingest every task
        // on each run (checkpoints move too); the two files are the record.
        let fingerprint_key = format!("kilocode:{}", task_id);
        let Some((mtime, size)) = task_fingerprint(&conversation_path, &ui_path) else {
            continue;
        };
        if reuse_ingested && store.is_ingested(&fingerprint_key, mtime, size)? {
            skipped += 1;
            continue;
        }
        let conversation = fs::read_to_string(&conversation_path)
            .with_context(|| format!("read {}", conversation_path.display()))?;
        let ui_messages =
            fs::read_to_string(&ui_path).with_context(|| format!("read {}", ui_path.display()))?;
        let records = kilocode_mod::parse_session(&conversation, &ui_messages);
        let spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);
        let usage = extract_usage_events(&records);
        // The root is the stated heuristic (`adapter::kilocode::estimate_root`):
        // kilocode stamps no cwd anywhere, so the common parent directory of
        // the leading tool paths is the best answer — empty when none appears.
        let root = kilocode_mod::estimate_root(&conversation).unwrap_or_default();
        let meta = SessionMeta {
            id: task_id.clone(),
            project: root.clone(),
            slug: task_id.clone(),
            root,
            source_path: fingerprint_key.clone(),
            sub_tokens: 0,
            sub_agent_count: 0,
            start: session_start(&records),
        };
        store.ingest_session(&meta, &spans, &usage)?;
        // `raw_errors` owns the strings; `error_rows` borrows them.
        let raw_errors = kilocode_mod::extract_tool_errors(&conversation);
        let error_rows: Vec<(i64, &str, &str, &str, &str)> = raw_errors
            .iter()
            .map(|error| {
                (
                    error.epoch_ms,
                    error.category.label(),
                    error.excerpt.as_str(),
                    error.tool.as_str(),
                    error.target.as_str(),
                )
            })
            .collect();
        store.ingest_tool_errors(&meta.id, &meta.source_path, &error_rows)?;
        // `work` owns the strings; the rows borrow them.
        let work = kilocode_mod::extract_work_events(&conversation);
        let work_rows: Vec<(i64, &str, &str, Option<&str>)> = work
            .iter()
            .map(|event| {
                (
                    event.epoch_ms,
                    event.kind,
                    event.id.as_str(),
                    event.path.as_deref(),
                )
            })
            .collect();
        store.ingest_work_events(&meta.id, &meta.source_path, &work_rows)?;
        // Record the size actually consumed, so a file that grows after the
        // read shows a mismatch next run instead of being skipped forever.
        store.record_ingested_file(&fingerprint_key, mtime, size)?;
        sessions += 1;
        spans_total += spans.len();
    }

    // Catalog the installed config — global scope plus every known project
    // root that still exists on disk — so usage joins against what is
    // installed. Skills join from the shared sites (deduped by canonical
    // path); MCP and permissions stay out of v1 (see
    // `docs/specs/config-format-kilocode.md`), so their live usage still
    // shows as orphaned rows in `inventory`.
    let config_dir = kilo_home()?;
    let home = home_dir().map(Path::new);
    let mut seen = std::collections::HashSet::new();
    let mut surfaces = kilocode_mod::read_global_surfaces(&config_dir, home, &mut seen);
    for (root, project) in store.session_roots()? {
        if Path::new(&root).is_dir() {
            surfaces.extend(kilocode_mod::read_project_surfaces(
                Path::new(&root),
                &project,
                home,
                &mut seen,
            ));
        }
    }
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    store.set_meta(
        kilocode_mod::KILOCODE_ANALYZER_META_KEY,
        kilocode_mod::KILOCODE_ANALYZER_VERSION,
    )?;
    store.set_meta("analyzed_at", &chrono::Utc::now().to_rfc3339())?;
    store.set_meta("projects_dir", &input.display().to_string())?;
    store.set_meta("config_dir", &config_dir.display().to_string())?;
    store.set_meta("harness", Harness::Kilocode.label())?;

    Ok(AnalyzeStats {
        sessions,
        skipped,
        spans_total,
        surface_count,
        denials: 0,
    })
}

/// The kilocode tasks directory for this run: the first editor globalStorage
/// candidate that exists (`adapter::harness::kilocode_tasks_candidates` is the
/// pure candidate list this read probes).
fn default_kilocode_tasks() -> Result<PathBuf> {
    let candidates = Harness::kilocode_tasks_candidates(home_dir().map(Path::new));
    let found = candidates.iter().find(|dir| dir.is_dir());
    match found {
        Some(dir) => Ok(dir.clone()),
        None => anyhow::bail!(
            "no kilocode tasks directory under any known editor — is the kilocode extension installed (or pass its path via --projects)?"
        ),
    }
}

/// The two files' stat folded into one per-task fingerprint: the latest mtime
/// (either file moves on activity) and the summed size.
fn task_fingerprint(conversation: &Path, ui: &Path) -> Option<(i64, i64)> {
    let conversation = file_fingerprint(conversation)?;
    let ui = file_fingerprint(ui)?;
    Some((conversation.0.max(ui.0), conversation.1 + ui.1))
}

/// Open the store for a read command. Unless `--frozen`, the analysis is
/// refreshed first — incremental, so an up-to-date store costs one stat per
/// transcript — which makes stale reads structurally impossible (`optimize` has
/// always worked this way; the views now compose the same stage). A one-line
/// freshness header goes to **stderr** so piped stdout stays clean.
fn open_for_read(harness: &Harness, db: &Path, frozen: bool) -> Result<Store> {
    if !frozen {
        // Re-analyze against the roots the store was built from (recorded in
        // meta); a fresh db falls back to the defaults.
        let projects = match db.exists() {
            true => Store::open(db)
                .context("open store")?
                .meta("projects_dir")?
                .map(PathBuf::from),
            false => None,
        };
        let stats = run_analyze(harness, projects, db)?;
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "store: refreshed just now ({} {} re-analyzed, {} unchanged)",
                stats.sessions,
                harness.input_unit(),
                stats.skipped
            ))
        );
        return Store::open(db).context("open store");
    }

    let store = Store::open(db).context("open store")?;
    // No refresh ran, so no guard ran either — refuse a wrong-harness read
    // here rather than reporting another harness's rows as this one's.
    guard_harness(&store, harness, db)?;
    eprint_freshness(&freshness_line(store.meta("analyzed_at")?.as_deref()));
    Ok(store)
}

/// Warn when a report knows a subagent total but not its per-agent split.
///
/// A store written before runs were extracted keeps its session totals, and
/// `--frozen` skips the refresh that would fill the split in — so for that one
/// read the per-agent rows sum to zero against a nonzero total. Reporting the
/// gap is the store's contract; printing the total as if nothing were missing
/// is the substitution it forbids (`docs/specs/storage.md`). It goes to stderr
/// with the other staleness signals, so piped stdout and `--format json` stay
/// clean.
///
/// A *partial* split is not warned about here: it is a property of the data
/// that no refresh can change, so each report labels it in-band beside the
/// rows it qualifies (`SplitCoverage::partial_note`).
fn warn_missing_agent_split(coverage: &SplitCoverage) {
    if coverage.is_unavailable() {
        eprintln!(
            "{}",
            Style::stderr().warn(
                "store: per-agent subagent split unavailable (store predates it) \
                 — run `saniwalens analyze` (or drop --frozen) to fill it in"
            )
        );
    }
}

/// Print a freshness line to stderr — dimmed context normally, highlighted
/// when it carries a staleness hint the user should act on.
fn eprint_freshness(line: &str) {
    let style = Style::stderr();
    if line.contains("saniwalens analyze") {
        eprintln!("{}", style.warn(line));
    } else {
        eprintln!("{}", style.dim(line));
    }
}

/// The `--frozen` freshness header: how old the store is, with a refresh hint
/// once it is older than a day. This is what lets the user *notice* staleness.
fn freshness_line(analyzed_at: Option<&str>) -> String {
    let Some(parsed) = analyzed_at.and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
    else {
        return "store: freshness unknown (no analyze recorded) — run `saniwalens analyze`"
            .to_string();
    };
    let age_secs = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
    let hint = if age_secs >= 24 * 3600 {
        " — run `saniwalens analyze` (or drop --frozen) to refresh"
    } else {
        ""
    };
    format!(
        "store: analyzed {} ago (--frozen){hint}",
        humanize_age(age_secs)
    )
}

fn humanize_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// A file's `(mtime epoch secs, size)` — the cheap change detector for
/// incremental ingest. `None` when the file cannot be stat'ed (it is then
/// ingested unconditionally and not recorded).
fn file_fingerprint(path: &Path) -> Option<(i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((mtime, meta.len() as i64))
}

/// Fold a worktree checkout's path onto its parent repository, mirroring
/// `normalize_project`'s slug rule at path level: `/repo/.wt/feat-x` → `/repo`.
///
/// Either separator delimits the segment, so a cwd recorded on Windows
/// (`C:\repo\.wt\feat-x`) folds like a unix one. The slug rule keys on `--wt-`
/// and is separator-free, so without this the two halves of the same
/// normalization disagreed and a project folded by slug stayed split by root.
fn normalize_root(cwd: &str) -> String {
    const SEGMENT: &str = ".wt";
    for (idx, _) in cwd.match_indices(SEGMENT) {
        let has_parent = cwd[..idx].ends_with(['/', '\\']);
        let has_child = cwd[idx + SEGMENT.len()..].starts_with(['/', '\\']);
        if has_parent && has_child {
            // `idx` is at least 1 here, and the separator before it is one byte.
            let parent = &cwd[..idx - 1];
            // Directly under a filesystem root there is no parent to fold onto:
            // the separator *is* the root, so dropping it would turn `/.wt/x`
            // into `""` and `C:\.wt\x` into a drive-relative `C:`. A drive
            // prefix is exactly two bytes, which keeps an ordinary directory
            // whose name ends in a colon out of this branch.
            if parent.is_empty() || (parent.len() == 2 && parent.ends_with(':')) {
                return cwd[..idx].to_string();
            }
            return parent.to_string();
        }
    }
    cwd.to_string()
}

/// Every subagent run of a session, read from `<sessionId>/subagents/` beside
/// the main transcript: `agent-<agentId>.jsonl` holds the run's cost, and its
/// `.meta.json` sidecar names the agent type and the call that spawned it.
///
/// Runs come back linked (`link_runs`), so a nested run already knows the
/// main-thread spawn its tree started at. A run whose sidecar is missing keeps
/// an unknown agent type and only the turn-level join key — it still counts,
/// with the gap visible, rather than being guessed at or dropped.
fn subagent_runs(transcript: &Path) -> Vec<SubagentRun> {
    let Ok(entries) = fs::read_dir(subagents_dir(transcript)) else {
        return Vec::new();
    };
    let mut runs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let sidecar = fs::read_to_string(subagent_sidecar_path(&path))
            .ok()
            .and_then(|json| parse_subagent_sidecar(&json))
            .unwrap_or_default();
        let records = parse_session(&text);
        runs.push(SubagentRun {
            agent: sidecar.agent_type,
            agent_id: path
                .file_stem()
                .map(|stem| subagent_id_from_file_name(&stem.to_string_lossy()))
                .unwrap_or_default(),
            run_path: path.display().to_string(),
            tool_use_id: sidecar.tool_use_id,
            parent_agent_id: sidecar.parent_agent_id,
            spawn_depth: sidecar.spawn_depth.unwrap_or(1),
            model: sidecar.model,
            prompt_id: subagent_prompt_id(&text),
            out_tokens: output_tokens(&records),
            started_epoch_ms: records.first().map(|r| r.timestamp_ms).unwrap_or_default(),
            root_tool_use_id: None,
        });
    }
    // Directory order is filesystem-dependent; spawn order is not. Sorting keeps
    // one session's ingest byte-identical across machines and re-runs.
    runs.sort_by(|a, b| {
        a.started_epoch_ms
            .cmp(&b.started_epoch_ms)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    link_runs(&mut runs);
    runs
}

fn usage(
    harness: &Harness,
    by: Option<&str>,
    format: Format,
    frozen: bool,
    db: &Path,
) -> Result<()> {
    let store = open_for_read(harness, db, frozen)?;

    if let Some(by) = by {
        let bucket = Bucket::parse(by)
            .with_context(|| format!("unknown --by value '{by}' (year|month|week|day|hour)"))?;
        return usage_by_time(&store, bucket, format);
    }

    let skills = store.skill_usage()?;
    // Where the tokens actually go: main-thread skill output vs subagents. The
    // subagent figure is usually the larger by far — worth seeing before reading
    // the per-skill table.
    let main_out: i64 = skills.iter().map(|row| row.out_tokens).sum();
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    let agents = store.subagent_by_agent()?;
    // How much of the total the rows actually account for: a store that kept
    // totals for sessions whose transcripts were pruned before their runs were
    // extracted has rows for only part of it, and the rows must not pass for
    // the whole. It travels with the rows in every format.
    let agent_split = SplitCoverage::new(
        sub_tokens,
        sub_agents,
        agents.iter().map(|a| (a.runs, a.out_tokens)),
    );
    // Ahead of the format branches: a machine consumer reading an empty `agents`
    // beside a nonzero total needs the reason as much as a human does, and it
    // goes to stderr precisely so it can be given to both.
    warn_missing_agent_split(&agent_split);
    if format == Format::Json {
        return emit_json(&serde_json::json!({
            "tokens": {
                "main_out": main_out,
                "sub_tokens": sub_tokens,
                "sub_agents": sub_agents,
            },
            "agent_split": agent_split,
            "agents": agents,
            "skills": skills,
        }));
    }
    // A subagent total with neither table behind it is not "nothing happened" —
    // it is a store whose per-agent split has not been extracted yet, and saying
    // "no usage" there would be false.
    if skills.is_empty() && agents.is_empty() && sub_tokens == 0 {
        println!("no skill usage found — run `saniwalens analyze` first");
        return Ok(());
    }
    preamble(
        format,
        "What ran and what it cost. First the subagent bill per agent type\n\
         (costliest first), then how often each skill ran — output tokens written,\n\
         context growth while it ran, and wall-clock seconds (most-invoked first).\n\
         A section with nothing to show is omitted.",
    );
    println!(
        "tokens: main-thread skill output {main_out}, subagents {sub_tokens} ({sub_agents} agents)"
    );
    // Stated between the total and the table, so the table below is read as
    // the part it is.
    if let Some(note) = agent_split.partial_note() {
        println!("{note}");
    }
    println!();

    // The subagent figure is usually the larger one, so break it down by agent
    // type before the per-skill table: a spawn count alone cannot tell an
    // expensive agent from a cheap one that runs often.
    if !agents.is_empty() {
        let rows: Vec<Vec<String>> = agents
            .iter()
            .map(|row| {
                vec![
                    row.agent.clone().unwrap_or_else(|| "(unknown)".to_string()),
                    row.runs.to_string(),
                    row.out_tokens.to_string(),
                ]
            })
            .collect();
        render(
            &["agent", "runs", "out tokens"],
            &[Align::Left, Align::Right, Align::Right],
            &rows,
            format,
        );
        println!();
    }

    if skills.is_empty() {
        return Ok(());
    }
    let rows: Vec<Vec<String>> = skills
        .iter()
        .map(|row| {
            vec![
                row.skill.clone(),
                row.invocations.to_string(),
                row.out_tokens.to_string(),
                row.ctx_growth.to_string(),
                format!("{:.0}", row.duration_sec),
            ]
        })
        .collect();
    render(
        &["skill", "runs", "out tokens", "ctx growth", "seconds"],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Skill usage rolled up per time bucket (JST). A long span is assigned whole to
/// its start bucket (`docs/specs/cli.md`).
fn usage_by_time(store: &Store, bucket: Bucket, format: Format) -> Result<()> {
    let events = store.skill_event_costs()?;

    let mut totals: std::collections::BTreeMap<String, (i64, i64, i64, f64)> =
        std::collections::BTreeMap::new();
    for event in &events {
        let label = bucket_label(event.started_epoch, bucket, JST_OFFSET_SECS);
        let row = totals.entry(label).or_default();
        row.0 += 1;
        row.1 += event.out_tokens;
        row.2 += event.ctx_growth;
        row.3 += event.duration_sec;
    }

    if format == Format::Json {
        let buckets: Vec<serde_json::Value> = totals
            .iter()
            .map(|(label, (count, out_tokens, ctx_growth, duration_sec))| {
                serde_json::json!({
                    "bucket": label,
                    "count": count,
                    "out_tokens": out_tokens,
                    "ctx_growth": ctx_growth,
                    "duration_sec": duration_sec,
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "buckets": buckets }));
    }
    if events.is_empty() {
        println!("no skill usage found — run `saniwalens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "Skill activity per time bucket (times in JST). A long-running skill counts\n\
         whole in the bucket it started in, so fine buckets can show spikes.",
    );
    let rows: Vec<Vec<String>> = totals
        .into_iter()
        .map(|(label, (count, out_tokens, ctx_growth, duration_sec))| {
            vec![
                label,
                count.to_string(),
                out_tokens.to_string(),
                ctx_growth.to_string(),
                format!("{duration_sec:.0}"),
            ]
        })
        .collect();
    render(
        &["bucket", "runs", "out tokens", "ctx growth", "seconds"],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Read all installed config (global scope) into one surface list.
fn read_global_surfaces() -> Result<Vec<Surface>> {
    let config_dir = claude_home()?;
    let scope = Scope::Global;
    let mut surfaces = read_skill_surfaces(&config_dir.join("skills"), &scope);
    surfaces.extend(read_rule_surfaces(&config_dir.join("rules"), &scope));
    surfaces.extend(read_agent_surfaces(&config_dir.join("agents"), &scope));
    surfaces.extend(read_mcp_server_surfaces(
        &config_dir.join("mcp.json"),
        &scope,
    ));
    surfaces.extend(read_claude_md_surfaces(
        &[(&config_dir.join("CLAUDE.md"), "global")],
        &scope,
        home_dir().map(Path::new),
    ));
    Ok(surfaces)
}

/// Join the catalogued surfaces against usage: each installed surface with its
/// static cost and (for usage-measurable kinds) invocation count, unused ones
/// flagged. Usage with no matching surface is shown as orphaned.
fn inventory(
    harness: &Harness,
    filter: &ScopeFilter,
    format: Format,
    frozen: bool,
    db: &Path,
) -> Result<()> {
    let store = open_for_read(harness, db, frozen)?;
    let catalog: Vec<_> = store
        .effective_catalog()?
        .into_iter()
        .filter(|e| in_scope(filter, &e.scope, &e.project))
        .collect();
    let usage = store.usage_counts()?;

    if format == Format::Json {
        let catalogued: std::collections::HashSet<(&str, &str)> = catalog
            .iter()
            .map(|e| (e.kind.as_str(), e.id.as_str()))
            .collect();
        let mut items: Vec<serde_json::Value> = catalog
            .iter()
            .map(|entry| {
                let measurable = is_usage_measurable(&entry.kind);
                serde_json::json!({
                    "kind": entry.kind,
                    "id": entry.id,
                    "scope": entry.scope,
                    "project": if entry.project.is_empty() { None } else { Some(&entry.project) },
                    "static_tokens": entry.static_tokens,
                    "startup_tokens": entry.startup_tokens,
                    "uses": if measurable { Some(entry.uses) } else { None },
                    "load_mode": entry.load_mode,
                    "status": if measurable && entry.uses == 0 { "unused" }
                              else if !measurable { "catalog-only" } else { "" },
                })
            })
            .collect();
        if *filter == ScopeFilter::All {
            for (kind, id, count) in &usage {
                if !catalogued.contains(&(kind.as_str(), id.as_str())) {
                    items.push(serde_json::json!({
                        "kind": kind, "id": id, "scope": null, "project": null,
                        "static_tokens": null, "startup_tokens": null,
                        "uses": count, "load_mode": null,
                        "status": "orphaned",
                    }));
                }
            }
        }
        return emit_json(&serde_json::json!({ "surfaces": items }));
    }
    if catalog.is_empty() && usage.is_empty() {
        println!("nothing catalogued — run `saniwalens analyze` first");
        return Ok(());
    }

    preamble(format, harness.inventory_preamble());
    let mut rows: Vec<Vec<String>> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        // "unused" is only meaningful for usage-measurable kinds; catalog-only
        // kinds (rules, hooks, CLAUDE.md) emit no events, so 0 means nothing.
        let (uses_cell, status) = if measurable {
            (
                entry.uses.to_string(),
                if entry.uses == 0 { "UNUSED" } else { "" },
            )
        } else {
            ("-".to_string(), "(catalog-only)")
        };
        let tokens = |value: Option<i64>| value.map_or_else(|| "?".to_string(), |t| t.to_string());
        rows.push(vec![
            entry.kind.clone(),
            entry.id.clone(),
            scope_cell(&entry.scope, &entry.project),
            tokens(entry.startup_tokens),
            tokens(entry.static_tokens),
            uses_cell,
            entry.load_mode.clone(),
            status.to_string(),
        ]);
    }

    // Orphaned usage has no scope to filter on; it appears in the full view only.
    let catalogued: std::collections::HashSet<(&str, &str)> = catalog
        .iter()
        .map(|e| (e.kind.as_str(), e.id.as_str()))
        .collect();
    for (kind, id, count) in &usage {
        if *filter != ScopeFilter::All {
            break;
        }
        if !catalogued.contains(&(kind.as_str(), id.as_str())) {
            rows.push(vec![
                kind.clone(),
                id.clone(),
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                count.to_string(),
                "-".to_string(),
                "ORPHANED".to_string(),
            ]);
        }
    }

    render(
        &[
            "kind",
            "id",
            "scope",
            "start tok",
            "static tok",
            "uses",
            "load mode",
            "status",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
            Align::Left,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Whether a catalog row's `(scope, project)` passes the `--scope` filter.
fn in_scope(filter: &ScopeFilter, scope: &str, project: &str) -> bool {
    if scope == "global" {
        filter.includes_global()
    } else {
        filter.includes_project(project)
    }
}

/// A human-readable scope cell: `global`, or `project:<slug>`.
fn scope_cell(scope: &str, project: &str) -> String {
    if scope == "project" {
        format!("project:{project}")
    } else {
        scope.to_string()
    }
}

/// List optimization wedges, ranked by what removing each surface actually saves
/// at session start — real always-on tokens first, then MCP schemas, then
/// declutter-only (skills/agents, whose body is on-demand). This is the "where
/// and how to optimize" view, honest about which removals save context.
fn waste(
    harness: &Harness,
    filter: &ScopeFilter,
    format: Format,
    frozen: bool,
    db: &Path,
) -> Result<()> {
    let store = open_for_read(harness, db, frozen)?;
    let catalog: Vec<_> = store
        .effective_catalog()?
        .into_iter()
        .filter(|e| in_scope(filter, &e.scope, &e.project))
        .collect();

    struct Row<'a> {
        wedge: Wedge,
        kind: &'a str,
        id: &'a str,
        scope: &'a str,
        project: &'a str,
        uses: i64,
        savings: StartupSavings,
        /// What every session pays for having it installed.
        startup_tokens: Option<u64>,
        /// What its body costs when loaded — an invocation cost, never a saving.
        on_demand_tokens: Option<u64>,
    }

    let mut found: Vec<Row> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|tokens| tokens as u64);
        let startup_tokens = entry.startup_tokens.map(|tokens| tokens as u64);
        if let Some(wedge) = classify_wedge(
            measurable,
            load_mode,
            static_tokens,
            entry.uses,
            HEAVY_TOKENS,
        ) {
            found.push(Row {
                wedge,
                kind: &entry.kind,
                id: &entry.id,
                scope: &entry.scope,
                project: &entry.project,
                uses: entry.uses,
                savings: startup_savings(load_mode, startup_tokens),
                startup_tokens,
                on_demand_tokens: on_demand_tokens(load_mode, static_tokens),
            });
        }
    }

    // Rank by real startup savings: measured tokens (desc), then unmeasured MCP
    // schemas, then declutter-only.
    found.sort_by_key(|row| savings_rank(row.savings));

    if format == Format::Json {
        let wedges: Vec<serde_json::Value> = found
            .iter()
            .map(|row| {
                let (savings, savings_tokens) = match row.savings {
                    StartupSavings::Tokens(n) => ("tokens", Some(n)),
                    StartupSavings::UnknownSchema => ("unknown_schema", None),
                    StartupSavings::Declutter => ("declutter", None),
                };
                serde_json::json!({
                    "wedge": row.wedge.label(),
                    "kind": row.kind,
                    "id": row.id,
                    "scope": row.scope,
                    "project": if row.project.is_empty() { None } else { Some(row.project) },
                    "savings": savings,
                    "savings_tokens": savings_tokens,
                    "startup_tokens": row.startup_tokens,
                    "on_demand_tokens": row.on_demand_tokens,
                    "uses": if is_usage_measurable(row.kind) { Some(row.uses) } else { None },
                    "suggestion": savings_suggestion(row.wedge, row.savings, row.on_demand_tokens),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "wedges": wedges }));
    }
    if found.is_empty() {
        println!("nothing wasteful found — run `saniwalens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "Concrete cleanup opportunities in your config, ranked by what removing each\n\
         one actually saves at session start. A \"~\" figure is declutter-only — just\n\
         the description is loaded at startup; the body costs nothing until invoked.",
    );
    let rows: Vec<Vec<String>> = found
        .iter()
        .map(|row| {
            let uses_cell = if is_usage_measurable(row.kind) {
                row.uses.to_string()
            } else {
                "-".to_string()
            };
            vec![
                row.wedge.label().to_string(),
                format!("{}/{}", row.kind, row.id),
                scope_cell(row.scope, row.project),
                savings_cell(row.savings, row.startup_tokens),
                uses_cell,
                savings_suggestion(row.wedge, row.savings, row.on_demand_tokens),
            ]
        })
        .collect();
    render(
        &[
            "problem",
            "surface",
            "scope",
            "saves@start",
            "uses",
            "suggestion",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Left,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Sort key: real measured savings first (largest first), then unmeasured MCP
/// schemas, then declutter-only.
fn savings_rank(savings: StartupSavings) -> (u8, std::cmp::Reverse<u64>) {
    match savings {
        StartupSavings::Tokens(n) => (0, std::cmp::Reverse(n)),
        StartupSavings::UnknownSchema => (1, std::cmp::Reverse(0)),
        StartupSavings::Declutter => (2, std::cmp::Reverse(0)),
    }
}

fn savings_cell(savings: StartupSavings, startup_tokens: Option<u64>) -> String {
    match savings {
        StartupSavings::Tokens(n) => format!("{n}/sess"),
        StartupSavings::UnknownSchema => "schema?".to_string(),
        // Negligible, but measured: a description-loaded surface still costs its
        // description every session, and that is all removing it reclaims.
        StartupSavings::Declutter => format!("~{}/sess", startup_tokens.unwrap_or(0)),
    }
}

/// A suggestion honest about whether removal saves context or only declutters —
/// naming the body cost as an invocation cost so it is not read as a saving.
fn savings_suggestion(wedge: Wedge, savings: StartupSavings, on_demand: Option<u64>) -> String {
    let body = match on_demand {
        Some(n) => format!("body is {n} tok when invoked"),
        None => "body is on-demand".to_string(),
    };
    match savings {
        StartupSavings::Tokens(n) => format!("removing saves ~{n} tokens every session"),
        StartupSavings::UnknownSchema => {
            "disable: drops its tool schema from every session (real, unmeasured)".to_string()
        }
        StartupSavings::Declutter => match wedge {
            Wedge::Unused => format!("declutter only: {body}, ~no startup saving"),
            _ => format!("heavy only when invoked ({body}); rarely used"),
        },
    }
}

/// Main session transcripts: the `<sessionId>.jsonl` files directly under each
/// project slug directory (subagent transcripts live one level deeper and are
/// not main-thread usage).
fn main_transcripts(projects: &Path) -> Result<Vec<PathBuf>> {
    let mut transcripts = Vec::new();
    let slugs = fs::read_dir(projects)
        .with_context(|| format!("read projects dir {}", projects.display()))?;
    for slug in slugs.flatten() {
        if !slug.path().is_dir() {
            continue;
        }
        for entry in fs::read_dir(slug.path()).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                transcripts.push(path);
            }
        }
    }
    Ok(transcripts)
}

fn session_meta(
    transcript: &Path,
    root: String,
    sub_tokens: i64,
    sub_agent_count: i64,
    start: Option<SessionStart>,
) -> SessionMeta {
    let id = transcript
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string();
    let slug = transcript
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    SessionMeta {
        project: normalize_project(&slug),
        slug,
        id,
        root,
        source_path: transcript.display().to_string(),
        sub_tokens,
        sub_agent_count,
        start,
    }
}

/// Fold a worktree slug onto its parent project so usage is not split across
/// `...--wt-feat-x` directories (see `docs/specs/storage.md`).
fn normalize_project(slug: &str) -> String {
    match slug.split_once("--wt-") {
        Some((parent, _)) => parent.to_string(),
        None => slug.to_string(),
    }
}

fn default_projects_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("projects"))
}

/// The user's home directory, resolved once per run.
///
/// `HOME` is preferred, so unix behaves exactly as before; Windows does not set
/// it, hence `USERPROFILE`. The wrinkle is that a Windows shell like Git Bash
/// *does* export `HOME` — as a POSIX path (`/c/Users/…`) that a native binary
/// cannot resolve. Taking it at face value would send every lookup to a
/// directory that does not exist and report "no transcripts" instead of an
/// error, so on Windows a candidate has to name a real directory to win. The
/// check stays off on unix, where a home that does not exist is worth surfacing
/// as itself rather than silently falling through to another variable.
fn home_dir() -> Option<&'static str> {
    static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        ["HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
            .find(|value| !cfg!(windows) || Path::new(value).is_dir())
    })
    .as_deref()
}

/// The Claude Code config root for this run: the environment read that
/// `adapter::config::claude_config_dir` deliberately does not do, plus the
/// advice to show when neither variable names one.
fn claude_home() -> Result<PathBuf> {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    // What can go wrong differs by platform, so the advice does too: only
    // Windows requires the value to name a real directory, and telling a unix
    // user otherwise sends them looking for a problem they do not have.
    claude_config_dir(config_dir.as_deref(), home_dir().map(Path::new)).with_context(|| {
        if cfg!(windows) {
            "set CLAUDE_CONFIG_DIR, or HOME/USERPROFILE to an existing directory"
        } else {
            "set CLAUDE_CONFIG_DIR or HOME"
        }
    })
}

/// Resolve `--db`, defaulting to a **user-level** store rather than a
/// cwd-relative file. What gets analyzed is every transcript under
/// `~/.claude/projects` — one cross-project dataset — so a per-directory
/// default would build a separate copy of the same store per directory saniwalens
/// happens to run from, and leave a database inside unrelated Git working
/// trees.
///
/// State, not cache. The store accumulates rows whose source transcripts Claude
/// Code later prunes, at which point it is the only surviving record of those
/// sessions and nothing on disk can rebuild it (`storage.md`). A cache
/// directory is by definition somewhere a cleaner may empty without asking —
/// exactly the one thing this file cannot survive.
///
/// Pure over its inputs (the environment is read by the caller) so the
/// resolution is testable. Each harness resolves to its own file name, so a
/// store built by one harness is never reinterpreted as another's rows
/// (`docs/specs/harness.md`).
fn store_path(
    db: Option<PathBuf>,
    harness: &Harness,
    state_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(db) = db {
        return Ok(db);
    }
    // Both roots must be absolute, for the same reason: honoring a relative one
    // would reintroduce the cwd-relative store this default exists to avoid.
    // (For `XDG_STATE_HOME` the XDG spec says as much — a non-absolute value is
    // invalid and must be ignored.)
    let base = match state_home.filter(|dir| dir.is_absolute()) {
        Some(dir) => dir.to_path_buf(),
        None => home
            .filter(|dir| dir.is_absolute())
            .context("HOME is not set to an absolute path")?
            .join(".local")
            .join("state"),
    };
    Ok(base.join("saniwalens").join(harness.db_file_name()))
}

/// Move a pre-rename store into place. The store outlives the transcripts it
/// was parsed from (`storage.md`), so the rename must never orphan it: the
/// binary has been renamed twice, so the first open under the current name
/// moves whichever legacy directory exists — the intermediate `agentlens` or
/// the original `cclens` — renamed under the new name with its per-harness
/// files, never deleted, never copied over an existing store.
fn migrate_legacy_store(resolved: &Path) {
    const LEGACY_DIRS: [&str; 2] = ["agentlens", "cclens"];
    let Some(new_dir) = resolved.parent() else {
        return;
    };
    let Some(state_base) = new_dir.parent() else {
        return;
    };
    if new_dir.exists() {
        return;
    }
    let Some(legacy_dir) = LEGACY_DIRS
        .iter()
        .map(|name| state_base.join(name))
        .find(|dir| dir.is_dir())
    else {
        return; // nothing to move
    };
    if std::fs::rename(&legacy_dir, new_dir).is_err() {
        return; // a move we cannot make is not ours to force
    }
    // The per-harness files carry an old name too.
    let Ok(entries) = std::fs::read_dir(new_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str() else {
            continue;
        };
        let Some(rest) = LEGACY_DIRS
            .iter()
            .find_map(|prefix| stem.strip_prefix(prefix))
        else {
            continue;
        };
        let _ = std::fs::rename(entry.path(), new_dir.join(format!("saniwalens{rest}")));
    }
}

/// `store_path` plus the legacy-store move, testable without touching the
/// environment: the caller supplies both roots. An explicit `--db` is the
/// user's own choice — the rename is not ours to apply to it.
fn resolve_db_from(
    db: Option<&Path>,
    harness: &Harness,
    config_db: Option<&Path>,
    state_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    let explicit = db.or(config_db);
    let path = store_path(explicit.map(PathBuf::from), harness, state_home, home)?;
    // The legacy-store move only applies to the pure default resolution: an
    // explicit `--db` or a configured one is the user's own choice.
    if db.is_none() && config_db.is_none() {
        migrate_legacy_store(&path);
    }
    Ok(path)
}

/// `store_path` against the live environment.
fn resolve_db(db: Option<PathBuf>, harness: &Harness, config_db: Option<&Path>) -> Result<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    resolve_db_from(
        db.as_deref(),
        harness,
        config_db,
        state_home.as_deref(),
        home_dir().map(Path::new),
    )
}

/// Where the tool's own config file lives, if anywhere: under
/// `$XDG_CONFIG_HOME/saniwalens/`, else `~/.config/saniwalens/`. Pure over
/// its inputs like `store_path`, for the same reason. `None` means there is
/// no file to read, which means "behave exactly as before".
fn config_file_path(config_home: Option<&Path>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = config_home.filter(|dir| dir.is_absolute()) {
        return Some(dir.join("saniwalens/config.toml"));
    }
    home.filter(|dir| Path::new(dir).is_absolute())
        .map(|dir| Path::new(dir).join(".config/saniwalens/config.toml"))
}

/// Column alignment for the table renderer.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// Output format for reports: `table` and `markdown` are for humans, `json`
/// is the machine contract — typed values, nothing but JSON on stdout (all
/// meta/freshness chatter goes to stderr).
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Table,
    Markdown,
    Json,
}

fn parse_format(value: Option<&str>) -> Result<Format> {
    match value.unwrap_or("table") {
        "table" => Ok(Format::Table),
        "markdown" | "md" => Ok(Format::Markdown),
        "json" => Ok(Format::Json),
        other => anyhow::bail!("unknown --format '{other}' (table|markdown|json)"),
    }
}

/// Print a machine-readable payload: pretty JSON, alone on stdout.
fn emit_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// ANSI styling for the human formats. Colors turn on only when the target is
/// a terminal, `NO_COLOR` is unset, and `TERM` is not `dumb` — so pipes,
/// captures, and `--format markdown|json` always get plain bytes.
#[derive(Clone, Copy)]
struct Style {
    on: bool,
}

impl Style {
    fn stdout() -> Self {
        use std::io::IsTerminal;
        Self {
            on: use_color(std::io::stdout().is_terminal()),
        }
    }

    fn stderr() -> Self {
        use std::io::IsTerminal;
        Self {
            on: use_color(std::io::stderr().is_terminal()),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Section headings (`== … ==`, table headers).
    fn heading(&self, text: &str) -> String {
        self.paint("1;36", text)
    }

    /// Pointers to follow-up commands.
    fn command(&self, text: &str) -> String {
        self.paint("36", text)
    }

    /// Attention: a finding's count, a wedge label, a staleness warning.
    fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    fn good(&self, text: &str) -> String {
        self.paint("32", text)
    }
}

fn use_color(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// Render a table as aligned text or GitHub-flavored markdown. Auto-sizes
/// columns; `aligns` controls per-column alignment in table mode. `json` is
/// each command's own payload, emitted before reaching here.
fn render(headers: &[&str], aligns: &[Align], rows: &[Vec<String>], format: Format) {
    match format {
        Format::Json => unreachable!("json is emitted by the command, not the table renderer"),
        Format::Markdown => {
            println!("| {} |", headers.join(" | "));
            let sep: Vec<&str> = headers.iter().map(|_| "---").collect();
            println!("| {} |", sep.join(" | "));
            for row in rows {
                println!("| {} |", row.join(" | "));
            }
        }
        Format::Table => {
            // Styling wraps a cell only after padding, so ANSI codes never
            // enter the width math.
            let style = Style::stdout();
            let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
            for row in rows {
                for (col, cell) in row.iter().enumerate() {
                    widths[col] = widths[col].max(cell.chars().count());
                }
            }
            let format_row = |cells: &[String]| -> String {
                cells
                    .iter()
                    .enumerate()
                    .map(|(col, cell)| pad(cell, widths[col], aligns[col]))
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
            println!("{}", style.heading(&format_row(&header_cells)));
            let total: usize = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
            println!("{}", style.dim(&"-".repeat(total)));
            for row in rows {
                println!("{}", format_row(row));
            }
        }
    }
}

fn pad(text: &str, width: usize, align: Align) -> String {
    let fill = " ".repeat(width.saturating_sub(text.chars().count()));
    match align {
        Align::Left => format!("{text}{fill}"),
        Align::Right => format!("{fill}{text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_prints_the_crate_version() {
        use clap::CommandFactory;
        let err = Cli::command()
            .try_get_matches_from(["saniwalens", "--version"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        // The binary reports the build-stamped version (git tag when built
        // from a checkout, the tag-stamped placeholder otherwise), never the
        // raw `0.0.0` placeholder a source build used to print.
        assert_eq!(
            err.to_string(),
            format!("saniwalens {}\n", env!("SANIWALENS_VERSION"))
        );
        assert_ne!(env!("SANIWALENS_VERSION"), "0.0.0");
    }

    fn agent(name: &str, runs: i64, out_tokens: i64) -> optimize_mod::AgentCost {
        optimize_mod::AgentCost {
            agent: Some(name.to_string()),
            runs,
            out_tokens,
        }
    }

    fn findings_with_agents(
        sub_tokens: i64,
        sub_agents: i64,
        agents: Vec<optimize_mod::AgentCost>,
    ) -> optimize_mod::Findings {
        optimize_mod::Findings {
            sub_tokens,
            sub_agents,
            agent_split: SplitCoverage::new(
                sub_tokens,
                sub_agents,
                agents.iter().map(|a| (a.runs, a.out_tokens)),
            ),
            agents,
            ..Default::default()
        }
    }

    #[test]
    fn doctor_calls_the_top_rows_most_of_the_output_only_when_they_are() {
        let f = findings_with_agents(
            1_000_000,
            10,
            vec![agent("a", 4, 700_000), agent("b", 6, 300_000)],
        );
        let lines = doctor_agent_lines(&f, "saniwalens usage");
        assert_eq!(
            lines,
            vec![
                "Most of that subagent output: a 700.0k (4 runs), b 300.0k (6 runs). \
                 Full split: saniwalens usage"
            ]
        );
    }

    #[test]
    fn doctor_does_not_call_a_minority_of_the_output_most_of_it() {
        // A long tail: ten types at a tenth each, so the three shown cover 30%
        // of the output, and "most" would be false even with a complete split.
        let agents: Vec<_> = (0..10)
            .map(|i| agent(&format!("t{i}"), 1, 100_000))
            .collect();
        let f = findings_with_agents(1_000_000, 10, agents);
        let lines = doctor_agent_lines(&f, "saniwalens usage");
        assert_eq!(
            lines,
            vec![
                "The largest agent types: t0 100.0k (1 run), t1 100.0k (1 run), t2 100.0k \
                 (1 run). Full split: saniwalens usage",
                "… and 7 more agent type(s) not shown",
            ]
        );
    }

    #[test]
    fn doctor_labels_a_partial_split_before_naming_its_agents() {
        // The rows explain 12% of the total: saying "most of that output" here
        // would blame the listed types for tokens no row accounts for.
        let f = findings_with_agents(
            3_093_466,
            375,
            vec![
                agent("general-purpose", 11, 315_663),
                agent("codex:codex-rescue", 14, 62_067),
            ],
        );
        let lines = doctor_agent_lines(&f, "saniwalens usage");
        assert_eq!(
            lines,
            vec![
                "Only 377.7k of those tokens (25 of 375 runs) have a per-agent split — the \
                 rest were analyzed before runs were extracted and their transcripts are gone.",
                "Among the runs that have one: general-purpose 315.7k (11 runs), \
                 codex:codex-rescue 62.1k (14 runs). Full split: saniwalens usage",
            ]
        );
    }

    #[test]
    fn doctor_names_no_agents_when_the_split_is_unavailable() {
        // The gap is reported on stderr with its refresh hint; there is
        // nothing to list in-band.
        let f = findings_with_agents(3_093_466, 375, vec![]);
        assert!(doctor_agent_lines(&f, "saniwalens usage").is_empty());
    }

    /// A path in the temp dir that no other test uses, removed on drop so a
    /// failure does not leak a file into the runner's temp dir.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("saniwalens-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn write_private_file_refuses_a_path_that_already_exists() {
        // The temp dir is shared, so a same-user process can pre-create the
        // path — as a symlink to somewhere it can read. Writing the briefing
        // into whatever is already there is the leak this prevents.
        let path = TempPath::new("preexisting.md");
        fs::write(&path.0, "squatted").unwrap();
        assert!(write_private_file(&path.0, "briefing").is_err());
        assert_eq!(fs::read_to_string(&path.0).unwrap(), "squatted");
    }

    #[test]
    fn write_private_file_writes_the_contents_to_a_fresh_path() {
        let path = TempPath::new("fresh.md");
        write_private_file(&path.0, "briefing").unwrap();
        assert_eq!(fs::read_to_string(&path.0).unwrap(), "briefing");
    }

    #[cfg(unix)]
    #[test]
    fn write_private_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let path = TempPath::new("mode.md");
        write_private_file(&path.0, "briefing").unwrap();
        let mode = fs::metadata(&path.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn shorten_to_tilde_replaces_the_home_prefix() {
        assert_eq!(
            shorten_to_tilde("/tmp/example/me/.claude", Some("/tmp/example/me")),
            "~/.claude"
        );
        // Home itself collapses to a bare tilde.
        assert_eq!(
            shorten_to_tilde("/tmp/example/me", Some("/tmp/example/me")),
            "~"
        );
    }

    #[test]
    fn shorten_to_tilde_accepts_a_backslash_separator() {
        assert_eq!(
            shorten_to_tilde(r"C:\example\me\.claude", Some(r"C:\example\me")),
            r"~\.claude"
        );
    }

    #[test]
    fn shorten_to_tilde_matches_a_home_spelled_with_the_other_separator() {
        // Windows takes either separator, and the home comes from the
        // environment while the path comes from a recorded cwd — so the two can
        // disagree on spelling while naming the same directory.
        assert_eq!(
            shorten_to_tilde("C:/example/me/.claude", Some(r"C:\example\me")),
            "~/.claude"
        );
        assert_eq!(
            shorten_to_tilde(r"C:\example\me\.claude", Some("C:/example/me")),
            r"~\.claude"
        );
        // The suffix keeps the spelling the path actually had, not the folded
        // one used for the comparison.
        assert_eq!(
            shorten_to_tilde(r"C:\example\me\a/b\c", Some("C:/example/me")),
            r"~\a/b\c"
        );
    }

    #[test]
    fn shorten_to_tilde_leaves_a_path_outside_home_alone() {
        assert_eq!(
            shorten_to_tilde("/tmp/other", Some("/tmp/example/me")),
            "/tmp/other"
        );
        // A sibling that merely shares the prefix is not under home.
        assert_eq!(
            shorten_to_tilde("/tmp/example/metadata", Some("/tmp/example/me")),
            "/tmp/example/metadata"
        );
    }

    #[test]
    fn shorten_to_tilde_passes_the_path_through_without_a_home() {
        assert_eq!(shorten_to_tilde("/tmp/example/me", None), "/tmp/example/me");
        assert_eq!(
            shorten_to_tilde("/tmp/example/me", Some("")),
            "/tmp/example/me"
        );
    }

    #[test]
    fn normalize_root_folds_a_worktree_path_onto_its_repo() {
        assert_eq!(
            normalize_root("/tmp/example/repo/.wt/feat-x"),
            "/tmp/example/repo"
        );
        // Idempotent: an already-folded root stays put.
        assert_eq!(normalize_root("/tmp/example/repo"), "/tmp/example/repo");
    }

    #[test]
    fn normalize_root_folds_a_backslash_worktree_path() {
        // A cwd recorded on Windows spells the same layout natively; leaving it
        // unfolded splits a project whose slug already folded.
        assert_eq!(
            normalize_root(r"C:\example\repo\.wt\feat-x"),
            r"C:\example\repo"
        );
        assert_eq!(normalize_root(r"C:\example\repo"), r"C:\example\repo");
    }

    #[test]
    fn normalize_root_keeps_the_filesystem_root_separator() {
        // Folding onto the root leaves the separator standing: it *is* the
        // parent. Dropping it would hand back a relative path — `""` on unix,
        // the drive-relative `C:` on Windows — and the root then names a
        // different directory than the one the session ran in.
        assert_eq!(normalize_root("/.wt/feat-x"), "/");
        assert_eq!(normalize_root(r"C:\.wt\feat-x"), r"C:\");
        // A directory whose name merely ends in a colon is not a drive.
        assert_eq!(normalize_root("/tmp/odd:/.wt/feat-x"), "/tmp/odd:");
    }

    #[test]
    fn normalize_root_leaves_a_dot_wt_that_is_not_a_segment_alone() {
        // Only a whole `.wt` directory is a worktree parent.
        assert_eq!(
            normalize_root("/tmp/example/repo/x.wt/feat-x"),
            "/tmp/example/repo/x.wt/feat-x"
        );
        // A trailing `.wt` names no checkout to fold onto.
        assert_eq!(
            normalize_root("/tmp/example/repo/.wt"),
            "/tmp/example/repo/.wt"
        );
    }

    #[test]
    fn a_declutter_row_shows_the_description_it_would_reclaim() {
        // The measured startup cost, not the body — deleting an unused skill
        // frees its description, and that is what the column promises.
        assert_eq!(
            savings_cell(StartupSavings::Declutter, Some(7)),
            "~7/sess".to_string()
        );
        assert_eq!(
            savings_cell(StartupSavings::Tokens(922), Some(922)),
            "922/sess".to_string()
        );
    }

    #[test]
    fn an_unused_body_is_suggested_as_an_invocation_cost_not_a_saving() {
        let suggestion = savings_suggestion(Wedge::Unused, StartupSavings::Declutter, Some(1015));
        assert!(suggestion.contains("1015 tok when invoked"));
        assert!(suggestion.contains("declutter only"));
    }

    #[test]
    fn store_path_defaults_under_the_user_state_dir() {
        assert_eq!(
            store_path(
                None,
                &Harness::Claude,
                Some(Path::new("/tmp/example/state")),
                None
            )
            .unwrap(),
            PathBuf::from("/tmp/example/state/saniwalens/saniwalens.db")
        );
        assert_eq!(
            store_path(
                None,
                &Harness::Claude,
                None,
                Some(Path::new("/tmp/example/home"))
            )
            .unwrap(),
            PathBuf::from("/tmp/example/home/.local/state/saniwalens/saniwalens.db")
        );
    }

    #[test]
    fn store_path_gives_each_harness_its_own_file() {
        // One store per harness: an opencode run never lands in the Claude
        // file, so an old store is never reinterpreted as another harness.
        assert_eq!(
            store_path(
                None,
                &Harness::Opencode,
                Some(Path::new("/tmp/example/state")),
                None
            )
            .unwrap(),
            PathBuf::from("/tmp/example/state/saniwalens/saniwalens-opencode.db")
        );
    }

    #[test]
    fn store_path_ignores_a_relative_xdg_state_home() {
        // XDG: a non-absolute XDG_STATE_HOME is invalid and must be ignored —
        // honoring it would put the store back under the current directory.
        assert_eq!(
            store_path(
                None,
                &Harness::Claude,
                Some(Path::new("relative/state")),
                Some(Path::new("/tmp/example/home"))
            )
            .unwrap(),
            PathBuf::from("/tmp/example/home/.local/state/saniwalens/saniwalens.db")
        );
    }

    #[test]
    fn store_path_without_a_home_is_an_error() {
        assert!(store_path(None, &Harness::Claude, None, None).is_err());
    }

    #[test]
    fn store_path_refuses_a_relative_home() {
        // The fallback root is held to the same bar as XDG_STATE_HOME: a
        // relative one would put the store back under the current directory.
        assert!(
            store_path(
                None,
                &Harness::Claude,
                None,
                Some(Path::new("relative/home"))
            )
            .is_err()
        );
    }

    #[test]
    fn store_path_takes_an_explicit_db_as_is() {
        // The escape hatch for a deliberately project-local store.
        assert_eq!(
            store_path(
                Some(PathBuf::from("./saniwalens.db")),
                &Harness::Opencode,
                Some(Path::new("/tmp/example/state")),
                Some(Path::new("/tmp/example/home"))
            )
            .unwrap(),
            PathBuf::from("./saniwalens.db")
        );
    }

    #[test]
    fn first_open_moves_a_legacy_store_and_renames_its_files() {
        // The store outlives the transcripts it was parsed from, so the
        // rename must never orphan it: the pre-rename directory moves under
        // the new name, its per-harness files included — never deleted.
        let dir =
            std::env::temp_dir().join(format!("saniwalens-migrate-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cclens")).unwrap();
        std::fs::write(dir.join("cclens/cclens.db"), b"rows").unwrap();
        std::fs::write(dir.join("cclens/cclens-opencode.db"), b"rows").unwrap();

        migrate_legacy_store(&dir.join("saniwalens/saniwalens.db"));

        assert_eq!(
            std::fs::read(dir.join("saniwalens/saniwalens.db")).unwrap(),
            b"rows"
        );
        assert_eq!(
            std::fs::read(dir.join("saniwalens/saniwalens-opencode.db")).unwrap(),
            b"rows"
        );
        assert!(!dir.join("cclens").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_open_moves_an_intermediate_legacy_store_too() {
        // The binary has been renamed twice: a store living under the
        // intermediate name moves as well, whichever legacy name it uses.
        let dir = std::env::temp_dir().join(format!(
            "saniwalens-migrate-mid-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("agentlens")).unwrap();
        std::fs::write(dir.join("agentlens/agentlens.db"), b"rows").unwrap();
        std::fs::write(dir.join("agentlens/agentlens-kilocode.db"), b"rows").unwrap();

        migrate_legacy_store(&dir.join("saniwalens/saniwalens.db"));

        assert_eq!(
            std::fs::read(dir.join("saniwalens/saniwalens.db")).unwrap(),
            b"rows"
        );
        assert_eq!(
            std::fs::read(dir.join("saniwalens/saniwalens-kilocode.db")).unwrap(),
            b"rows"
        );
        assert!(!dir.join("agentlens").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_migration_never_touches_an_existing_store() {
        // A move never overwrites: a store already living under the new name
        // stays, and the legacy one survives beside it rather than being
        // deleted to make room.
        let dir = std::env::temp_dir().join(format!(
            "saniwalens-migrate-keep-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cclens")).unwrap();
        std::fs::write(dir.join("cclens/cclens.db"), b"legacy").unwrap();
        std::fs::create_dir_all(dir.join("saniwalens")).unwrap();
        std::fs::write(dir.join("saniwalens/saniwalens.db"), b"current").unwrap();

        migrate_legacy_store(&dir.join("saniwalens/saniwalens.db"));

        assert_eq!(
            std::fs::read(dir.join("saniwalens/saniwalens.db")).unwrap(),
            b"current"
        );
        assert_eq!(
            std::fs::read(dir.join("cclens/cclens.db")).unwrap(),
            b"legacy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_migration_is_silent_when_nothing_is_to_move() {
        let dir = std::env::temp_dir().join(format!(
            "saniwalens-migrate-none-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        migrate_legacy_store(&dir.join("saniwalens/saniwalens.db"));

        assert!(!dir.join("saniwalens").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rename_applies_only_to_the_default_resolution() {
        // An explicit `--db` is the user's own choice — the rename is not
        // ours to apply to it. The default resolution moves the legacy store
        // into place under the new name.
        let dir =
            std::env::temp_dir().join(format!("saniwalens-migrate-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cclens")).unwrap();
        std::fs::write(dir.join("cclens/cclens.db"), b"rows").unwrap();
        let explicit = dir.join("chosen.db");

        let resolved = resolve_db_from(
            Some(&explicit),
            &Harness::Claude,
            None,
            Some(&dir),
            Some(&dir),
        )
        .unwrap();

        assert_eq!(resolved, explicit);
        assert!(dir.join("cclens/cclens.db").exists());

        let default =
            resolve_db_from(None, &Harness::Claude, None, Some(&dir), Some(&dir)).unwrap();

        assert_eq!(default, dir.join("saniwalens/saniwalens.db"));
        assert!(default.exists());
        assert!(!dir.join("cclens").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_db_fills_in_only_when_no_explicit_db() {
        // The file's db becomes the store, but an explicit `--db` still wins
        // and the legacy-store move stays out of both.
        let dir =
            std::env::temp_dir().join(format!("saniwalens-config-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let from_file = dir.join("file.db");
        let explicit = dir.join("chosen.db");

        let resolved = resolve_db_from(
            None,
            &Harness::Claude,
            Some(&from_file),
            Some(&dir),
            Some(&dir),
        )
        .unwrap();
        assert_eq!(resolved, from_file);

        let resolved = resolve_db_from(
            Some(&explicit),
            &Harness::Claude,
            Some(&from_file),
            Some(&dir),
            Some(&dir),
        )
        .unwrap();
        assert_eq!(resolved, explicit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_harness_defaults_to_claude_and_rejects_the_rest() {
        assert_eq!(parse_harness(None).unwrap(), Harness::Claude);
        assert_eq!(parse_harness(Some("opencode")).unwrap(), Harness::Opencode);
        assert!(parse_harness(Some("auto")).is_err());
    }

    #[test]
    fn parse_profile_defaults_to_none() {
        // --profile is optional and global, like --harness.
        let cli = Cli::try_parse_from(["saniwalens", "doctor"]).unwrap();
        assert_eq!(cli.profile, None);
        let cli = Cli::try_parse_from(["saniwalens", "--profile", "work", "doctor"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
    }

    #[test]
    fn config_file_path_defaults_under_the_user_config_dir() {
        assert_eq!(
            config_file_path(Some(Path::new("/tmp/example/config")), None),
            Some(PathBuf::from("/tmp/example/config/saniwalens/config.toml"))
        );
        assert_eq!(
            config_file_path(None, Some("/tmp/example/home")),
            Some(PathBuf::from(
                "/tmp/example/home/.config/saniwalens/config.toml"
            ))
        );
    }

    #[test]
    fn config_file_path_ignores_a_relative_xdg_config_home() {
        // A relative XDG_CONFIG_HOME is invalid and must be ignored, for the
        // same reason store_path ignores a relative XDG_STATE_HOME.
        assert_eq!(
            config_file_path(Some(Path::new("relative/config")), None),
            None
        );
    }

    #[test]
    fn config_file_path_without_a_home_is_none() {
        // No roots means no file to read, which means "behave as before".
        assert_eq!(config_file_path(None, None), None);
    }

    #[test]
    fn launch_command_runs_each_harness_own_agent() {
        // `claude` opens interactively on the prompt; `opencode run` runs the
        // advisor headless on JSON events (raw tool dumps loggable) with a
        // recognizable session title (see `run_instructions`). kilo is an
        // OpenCode fork and documents `kilo run [message..]` with `--format
        // json` (raw JSON events) and `--title` — the same surface, so the
        // launch is the opencode shape under kilo's binary.
        assert_eq!(
            launch_command(&Harness::Claude),
            Some(("claude", &[] as &[&str], optimize_mod::RunMode::Interactive))
        );
        assert_eq!(
            launch_command(&Harness::Opencode),
            Some((
                "opencode",
                &["run", "--format", "json", "--title", "saniwalens optimize"] as &[&str],
                optimize_mod::RunMode::Headless
            ))
        );
        assert_eq!(
            launch_command(&Harness::Kilocode),
            Some((
                "kilo",
                &["--prompt"] as &[&str],
                optimize_mod::RunMode::Interactive
            ))
        );
    }

    #[test]
    fn guard_harness_adopts_a_store_without_a_key() {
        // A store predating the flag carries no `harness` key and is adopted
        // by the Claude default, never refused.
        let mut store = Store::in_memory().unwrap();
        assert!(guard_harness(&store, &Harness::Claude, Path::new("/tmp/example/x.db")).is_ok());
        store.set_meta("harness", Harness::Claude.label()).unwrap();
        assert!(guard_harness(&store, &Harness::Claude, Path::new("/tmp/example/x.db")).is_ok());
        assert!(guard_harness(&store, &Harness::Opencode, Path::new("/tmp/example/x.db")).is_err());
    }

    #[test]
    fn frozen_reads_refuse_a_wrong_harness_store() {
        // `--frozen` skips the analyze that guards, so the read path guards
        // itself — otherwise a Claude read would report opencode rows.
        let path = TempPath::new("guard.db");
        {
            let mut store = Store::open(&path.0).unwrap();
            store
                .set_meta("harness", Harness::Opencode.label())
                .unwrap();
        }
        assert!(open_for_read(&Harness::Opencode, &path.0, true).is_ok());
        assert!(open_for_read(&Harness::Claude, &path.0, true).is_err());
    }
}
