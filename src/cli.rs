//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::harness::{Harness, home_dir};
use crate::adapter::opencode as opencode_mod;
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::findings;
use crate::core::friction::ErrorCategory;
use crate::core::optimize as optimize_mod;
use crate::core::period::{self, TimeRange, WindowPair};
use crate::core::scope::{ScopeFilter, split_friction};
use crate::core::subagent::SplitCoverage;
use crate::core::surface::{
    HEAVY_TOKENS, LoadMode, StartupSavings, Wedge, classify_wedge, is_usage_measurable,
    on_demand_tokens, startup_savings,
};
use crate::core::thrash::detect_thrash;
use crate::session::{
    AnalyzeStats, Session, SessionEnv, Style, eprint_freshness, freshness_line, guard_harness,
    resolve_cli,
};
use crate::store::{KindRollup, OpenMode, Store, TrendWindowAggregates, TwoWindow, skill_deltas};

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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
    /// How usage moved: each kind of event, each skill, and the session and
    /// subagent totals this window against the equal-length window right
    /// before it (default 7d vs the 7d before).
    Trend {
        /// Current window start (YYYY-MM-DD, JST). The window to compare it
        /// with is the same length immediately before it.
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Current window end (YYYY-MM-DD, JST) — the whole day counts.
        /// Needs --since; alone, the window to compare against is undefined.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling pair ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
        /// Output format: table | markdown | json.
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
        /// Only count events on or after this date (YYYY-MM-DD, JST).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Only count events through this date (YYYY-MM-DD, JST) — the
        /// whole day counts.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Rolling window ending now: <N>d | <N>h | <N>w (e.g. 7d). Exclusive
        /// with the explicit dates.
        #[arg(long, value_name = "SPAN", conflicts_with_all = ["since", "until"])]
        last: Option<String>,
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
    // One call resolves flags, the config file, and the builtins into the
    // harness plus an optional configured store; see `session.rs`.
    let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let env = SessionEnv {
        state_home: state_home.as_deref(),
        home: home_dir(),
        config_home: config_home.as_deref(),
    };
    let resolved = resolve_cli(cli.harness.as_deref(), cli.profile.as_deref(), &env)?;
    let harness = resolved.harness;
    let config_db = resolved.config_db;
    // Flags are parsed before the session is built — the pre-Session dispatch
    // evaluated `parse_scope`/`parse_format` ahead of any store work, so an
    // invalid flag fails on its own instead of surfacing a store error (or a
    // freshness line) first. Byte-identical output, error ordering included
    // (PBI 01's acceptance criterion).
    match cli.command {
        Command::Analyze {
            projects,
            format,
            db,
        } => {
            let fmt = parse_format(format.as_deref())?;
            let sess = Session::open(&harness, db, config_db.as_deref(), &env)?;
            analyze(&sess, projects, fmt)
        }
        Command::Usage {
            by,
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            usage(&sess, by.as_deref(), window, fmt)
        }
        Command::Trend {
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let fmt = parse_format(format.as_deref())?;
            // The pair resolves before the session opens — the same
            // ahead-of-store evaluation `parse_period` gives the other views,
            // so a bad date fails as a period error before any store work.
            let pair = parse_pair(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            trend(&sess, pair, fmt)
        }
        Command::Inventory {
            scope,
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let filter = parse_scope(scope.as_deref())?;
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            inventory(&sess, &filter, window, fmt)
        }
        Command::Waste {
            scope,
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let filter = parse_scope(scope.as_deref())?;
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            waste(&sess, &filter, window, fmt)
        }
        Command::Overhead {
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            overhead(&sess, window, fmt)
        }
        Command::Prompts {
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            prompts(&sess, window, fmt)
        }
        Command::Failures {
            scope,
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let filter = parse_scope(scope.as_deref())?;
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            failures(&sess, &filter, window, fmt)
        }
        Command::Stuck {
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            stuck(&sess, window, fmt)
        }
        Command::Doctor {
            scope,
            since,
            until,
            last,
            format,
            frozen,
            db,
        } => {
            let filter = parse_scope(scope.as_deref())?;
            let fmt = parse_format(format.as_deref())?;
            let window = parse_period(since.as_deref(), until.as_deref(), last.as_deref())?;
            let sess = Session::open_read(
                &harness,
                db,
                config_db.as_deref(),
                frozen,
                &env,
                run_analyze,
            )?;
            doctor(&sess, &filter, window, fmt)
        }
        Command::Sql { query, format, db } => {
            let fmt = parse_format(format.as_deref())?;
            let sess = Session::open_readonly(&harness, db, config_db.as_deref(), &env)?;
            sql(&sess, query.as_deref(), fmt)
        }
        Command::Optimize {
            projects,
            db,
            scope,
            frozen,
            print,
            verbose,
        } => {
            // Resolve (including the legacy-store move) before parsing --scope,
            // then open after — the pre-Session dispatch's exact order, kept so
            // double failures report the same error the old binary did.
            let path = Session::resolve_path(&harness, db, config_db.as_deref(), &env)?;
            let filter = parse_scope(scope.as_deref())?;
            let sess = Session::open_resolved(&harness, path)?;
            optimize(&sess, projects, &filter, frozen, print, verbose)
        }
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
) -> (&'static str, &'static [&'static str], optimize_mod::RunMode) {
    match harness {
        Harness::Claude => ("claude", &[], optimize_mod::RunMode::Interactive),
        Harness::Opencode => (
            "opencode",
            &["run", "--format", "json", "--title", "saniwalens optimize"],
            optimize_mod::RunMode::Headless,
        ),
        // The `kilo` binary is the measured agent CLI (Homebrew), and kilo is
        // an OpenCode fork whose TUI takes a prompt: `kilo [project] --prompt
        // <prompt>` opens the interactive session on the pointer prompt — the
        // experience the claude launch gives, with the user present to approve
        // the briefing read and steer (kilo gates every tool call where
        // opencode does not, so a headless run would need `--auto` instead).
        // A bare `kilo <text>` positional is treated as a file to open
        // (measured), not a prompt — `--prompt` is the TUI's message surface.
        Harness::Kilocode => ("kilo", &["--prompt"], optimize_mod::RunMode::Interactive),
    }
}

/// How the briefing reaches the advisor — the three deliveries `optimize`
/// picks between (`docs/specs/cli.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Interactive,
    Headless,
    Print,
}

/// `--print` wins over the harness's launch shape.
fn delivery_mode(print: bool, launch: optimize_mod::RunMode) -> DeliveryMode {
    if print {
        DeliveryMode::Print
    } else {
        match launch {
            optimize_mod::RunMode::Interactive => DeliveryMode::Interactive,
            optimize_mod::RunMode::Headless => DeliveryMode::Headless,
        }
    }
}

/// What gets delivered: the full prompt itself (`--print`), or the rendered
/// briefing behind a private pointer file whose argv prompt `pointer` builds
/// once the file's path exists.
enum DeliveryPayload<'a> {
    Full(String),
    Launch {
        briefing: String,
        pointer: Box<dyn FnOnce(&Path) -> String + 'a>,
    },
}

/// One delivery, assembled: mode, launch shape, payload. Built by `optimize`,
/// run by `deliver` (`pbi/2026-10-29-06-feat-optimize-delivery-seam.md`).
pub struct DeliveryPlan<'a> {
    mode: DeliveryMode,
    binary: &'static str,
    leading: &'static [&'static str],
    harness_name: &'static str,
    verbose: bool,
    payload: DeliveryPayload<'a>,
}

impl<'a> DeliveryPlan<'a> {
    /// `--print`: the prompt itself, destined for stdout — no process, no
    /// temp file (`docs/specs/cli.md`: the exception that opts the user into
    /// seeing the full prompt).
    pub fn print(prompt: String) -> Self {
        Self {
            mode: DeliveryMode::Print,
            binary: "",
            leading: &[],
            harness_name: "",
            verbose: false,
            payload: DeliveryPayload::Full(prompt),
        }
    }

    /// A launch: the briefing travels behind a private pointer file, and
    /// `pointer` assembles the argv prompt for that file's path.
    pub fn launch(
        binary: &'static str,
        leading: &'static [&'static str],
        mode: DeliveryMode,
        briefing: String,
        harness_name: &'static str,
        verbose: bool,
        pointer: impl FnOnce(&Path) -> String + 'a,
    ) -> Self {
        Self {
            mode,
            binary,
            leading,
            harness_name,
            verbose,
            payload: DeliveryPayload::Launch {
                briefing,
                pointer: Box::new(pointer),
            },
        }
    }
}

/// The process side of a launch — the adapter `deliver` runs through, so a
/// test substitutes a double and asserts the argv instead of spawning.
pub trait AdvisorLaunch {
    /// Spawn with stdio inherited: the interactive TUI, and `--verbose`'s raw
    /// streaming for a headless harness.
    fn direct(
        &mut self,
        binary: &str,
        leading: &[&str],
        prompt: &str,
    ) -> Result<std::process::ExitStatus>;

    /// Spawn headless: the raw stream goes to a private log, the agent's text
    /// to stdout. Returns the exit status and the log path.
    fn headless(
        &mut self,
        binary: &str,
        leading: &[&str],
        prompt: &str,
    ) -> Result<(std::process::ExitStatus, PathBuf)>;
}

/// The real process side: an inherited-stdio spawn, or `run_headless`'s
/// logged pipe.
struct ProcessAdvisor;

impl AdvisorLaunch for ProcessAdvisor {
    fn direct(
        &mut self,
        binary: &str,
        leading: &[&str],
        prompt: &str,
    ) -> Result<std::process::ExitStatus> {
        Ok(std::process::Command::new(binary)
            .args(leading)
            .arg(prompt)
            .status()?)
    }

    fn headless(
        &mut self,
        binary: &str,
        leading: &[&str],
        prompt: &str,
    ) -> Result<(std::process::ExitStatus, PathBuf)> {
        run_headless(binary, leading, prompt)
    }
}

/// Deliver `plan` — the one seam between the briefing and the advisor.
/// Launch modes write the briefing to a private (`0600`) pointer file, hand
/// the spawn to the `advisor` adapter, and remove the file the moment the run
/// ends whatever the outcome; `Print` writes the prompt to stdout. The
/// briefing itself never rides argv — a process argument is world-visible
/// (`ps`) — and only a data-free pointer does.
pub fn deliver(plan: DeliveryPlan<'_>, advisor: &mut impl AdvisorLaunch) -> Result<()> {
    let DeliveryPlan {
        mode,
        binary,
        leading,
        harness_name,
        verbose,
        payload,
    } = plan;
    let (prompt, path) = match payload {
        DeliveryPayload::Full(prompt) => {
            println!("{prompt}");
            return Ok(());
        }
        DeliveryPayload::Launch { briefing, pointer } => {
            let path = write_private_tempfile(&briefing).context("write briefing temp file")?;
            (pointer(&path), path)
        }
    };

    let launch_note = || {
        format!(
            "launch `{} {}` — is {} installed and on PATH?",
            binary,
            leading.join(" ").trim_end(),
            harness_name
        )
    };
    // Headless harnesses run piped unless --verbose: the raw stream is logged
    // to a private file and only the agent's text streams on stdout; verbose
    // keeps the raw streaming by handing the terminal over.
    let (result, run_log) = if mode == DeliveryMode::Headless && !verbose {
        match advisor.headless(binary, leading, &prompt) {
            Ok((status, log)) => (Ok(status), Some(log)),
            Err(err) => (Err(err), None),
        }
    } else {
        (advisor.direct(binary, leading, &prompt), None)
    };
    let _ = fs::remove_file(&path);

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
///
/// The briefing carries concrete paths and error excerpts that may be sensitive,
/// so it is written to a private (`0600`) temp file and only a pointer is passed
/// on argv — never the data, which `ps` would otherwise expose. The file is
/// removed as soon as the session ends. `--print` instead writes the full prompt
/// (briefing inline) to stdout for piping / inspection.
fn optimize(
    sess: &Session,
    projects: Option<PathBuf>,
    filter: &ScopeFilter,
    frozen: bool,
    print: bool,
    verbose: bool,
) -> Result<()> {
    if !frozen {
        let stats = run_analyze(&sess.harness, projects, &sess.db_path)?;
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "store: refreshed just now ({} {} re-analyzed, {} unchanged)",
                stats.sessions,
                sess.harness.input_unit(),
                stats.skipped
            ))
        );
    }
    // `optimize` takes the whole history — no period flags reach it.
    let findings = collect_findings(&sess.store, TimeRange::ALL)?;

    // The absolute store path so the session's `saniwalens sql --db …` hits the
    // store this run built, wherever it is launched from.
    let db_display = std::fs::canonicalize(&sess.db_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| sess.db_path.to_string_lossy().into_owned());

    // The harness copy for every prompt and briefing rendered here — the
    // harness assembles its own (adapter side), so the core stays agnostic.
    let copy = sess.harness.copy();
    let (binary, leading, run_mode) = launch_command(&sess.harness);

    // One seam from here: the payload (the full prompt, or the rendered
    // briefing behind a private pointer file) and the mode are decided, then
    // `deliver` runs them against the process adapter. `claude` opens an
    // interactive TUI, `opencode run` goes headless, kilo's TUI takes
    // `--prompt` (`docs/specs/harness.md`).
    let plan = match delivery_mode(print, run_mode) {
        DeliveryMode::Print => DeliveryPlan::print(optimize_mod::compose_prompt(
            &findings,
            &db_display,
            filter,
            &copy,
        )),
        mode => DeliveryPlan::launch(
            binary,
            leading,
            mode,
            optimize_mod::render_briefing(&findings, filter, &copy),
            sess.harness.display_name(),
            verbose,
            |path| {
                optimize_mod::launch_prompt(
                    &path.to_string_lossy(),
                    &db_display,
                    filter,
                    &copy,
                    run_mode,
                )
            },
        ),
    };
    deliver(plan, &mut ProcessAdvisor)?;
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

/// Gather the COMPLETE analysis the optimization briefing carries: fetch the
/// store's rows and hand them to the pure assembly in `core::findings`. The
/// shell's half is the queries and the store-row → plain-row mapping; every
/// join, cap, and sort lives in the core where it is testable
/// (`pbi/2026-10-29-04-feat-findings-assembly-core.md`).
fn collect_findings(store: &Store, window: TimeRange) -> Result<optimize_mod::Findings> {
    Ok(findings::collect_findings(&findings_input(store, window)?))
}

/// Every row `core::findings` assembles from, fetched in one place.
fn findings_input(store: &Store, window: TimeRange) -> Result<findings::FindingsInput> {
    Ok(findings::FindingsInput {
        // No session start observed means the always-on section is omitted
        // rather than guessed at — the sentinel stays the core's business.
        floor: store.baseline_floor(window)?,
        error_examples: store.error_examples(findings::EXAMPLES_PER_CATEGORY, window)?,
        error_breakdown: store.error_tool_breakdown(window)?,
        friction_cells: store.error_counts_by_project(window)?,
        bash_commands: store.work_counts("bash_cmd", window)?,
        file_edit_counts: store.work_counts("file_edit", window)?,
        file_edits: store.file_edits(window)?,
        catalog: store
            .effective_catalog(window)?
            .into_iter()
            .map(|e| findings::CatalogRow {
                kind: e.kind,
                id: e.id,
                scope: e.scope,
                project: e.project,
                startup_tokens: e.startup_tokens,
                static_tokens: e.static_tokens,
                load_mode: e.load_mode,
                uses: e.uses,
            })
            .collect(),
        session_roots: store.session_roots()?,
        prompt_behaviors: store.prompt_behavior_counts(window)?,
        subagent_totals: store.subagent_totals(window)?,
        agents: store
            .subagent_by_agent(window)?
            .into_iter()
            .map(|row| optimize_mod::AgentCost {
                agent: row.agent,
                runs: row.runs,
                out_tokens: row.out_tokens,
            })
            .collect(),
        skill_out_tokens: store
            .skill_usage(window)?
            .iter()
            .map(|r| r.out_tokens)
            .collect(),
        always_on_config_tokens: store.always_on_config_tokens()?,
    })
}

/// Run a read-only SQL query against the store and print the result. The query
/// comes from the argument or, when absent, from stdin — so both
/// `saniwalens sql '…'` and `echo '…' | saniwalens sql` work. The store is
/// opened read-only so an ad-hoc query can never mutate the derived data.
fn sql(sess: &Session, query: Option<&str>, format: Format) -> Result<()> {
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

    let store = &sess.store;
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
fn doctor(sess: &Session, filter: &ScopeFilter, window: TimeRange, format: Format) -> Result<()> {
    if format == Format::Markdown {
        anyhow::bail!("summary has no markdown form — use the default text or --format json");
    }
    let store = &sess.store;
    eprint_window(window);
    let f = collect_findings(store, window)?;

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
        return emit_json(&with_window(
            serde_json::json!({
                "global": global,
                "projects": projects,
            }),
            window,
        ));
    }

    // The text form is written for someone who has never seen saniwalens's
    // vocabulary: every item says what happened, why it costs, and what to do
    // — prioritized, so the report reads top to bottom as a to-do list.
    let style = Style::stdout();
    let (session_count, project_count) = store.session_stats(window)?;
    if session_count == 0 {
        println!(
            "{}",
            empty_in_window(
                "nothing analyzed yet — run `saniwalens analyze` first",
                window
            )
        );
        return Ok(());
    }
    println!(
        "{}",
        style.heading(&format!(
            "{} usage health check — {session_count} sessions across \
             {project_count} projects",
            sess.harness.display_name()
        ))
    );
    // The evaluation window, in-band for a text reader: the session count and
    // every section below are already window-filtered.
    if let Some(line) = window_line(window) {
        println!("{}", style.dim(&line));
    }

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
                    ErrorCategory::from_label(&cat.label)
                        .suggestion(sess.harness.instruction_file())
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
            sess.harness.project_config_ref(),
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
                                .suggestion(sess.harness.instruction_file())
                        ))
                    ),
                    format!(
                        "  No single project owns it, so the fix belongs in {} — \
                         details: {}",
                        sess.harness.global_config_ref(),
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
                    sess.harness.instruction_file()
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
                    sess.harness.display_name()
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
        warn_missing_agent_split(&f.agent_split, window);
        for line in doctor_agent_lines(&f, &style.command("saniwalens usage"), window) {
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
            sess.harness.global_config_ref(),
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
/// reported on stderr with its refresh hint. Under a drawn window the gap is
/// the two time axes at work, so the sentence names the window instead of the
/// pruned transcripts (`window.is_all()` keeps the full-history wording).
fn doctor_agent_lines(
    f: &optimize_mod::Findings,
    full_split: &str,
    window: TimeRange,
) -> Vec<String> {
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
        if window.is_all() {
            lines.push(format!(
                "Only {} of those tokens ({} of {} runs) have a per-agent split — the rest were \
                 analyzed before runs were extracted and their transcripts are gone.",
                fmt_tokens(coverage.known_tokens),
                coverage.known_runs,
                coverage.total_runs
            ));
        } else {
            lines.push(format!(
                "Only {} of those tokens ({} of {} runs) have a per-agent split — the rest \
                 fall outside the report window.",
                fmt_tokens(coverage.known_tokens),
                coverage.known_runs,
                coverage.total_runs
            ));
        }
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

/// Resolve the period flags before the session is built — the same
/// ahead-of-store evaluation `parse_scope`/`parse_format` have, so an
/// unparseable date fails on its own. The clock is read here: this is the
/// thin shell around the pure transform in `core::period`.
fn parse_period(since: Option<&str>, until: Option<&str>, last: Option<&str>) -> Result<TimeRange> {
    let now = chrono::Utc::now().timestamp();
    period::resolve(since, until, last, now, JST_OFFSET_SECS).map_err(anyhow::Error::msg)
}

/// Resolve the trend's period flags into the two comparison windows before
/// the session is built — the same ahead-of-store evaluation `parse_period`
/// gives the other views, so an unparseable date, a `--until` without a
/// `--since`, or an inverted window fails as a period error before any store
/// work or freshness line. The clock is read here: the thin shell around the
/// pure transform in `core::period`.
fn parse_pair(since: Option<&str>, until: Option<&str>, last: Option<&str>) -> Result<WindowPair> {
    let now = chrono::Utc::now().timestamp();
    period::resolve_pair(since, until, last, now, JST_OFFSET_SECS).map_err(anyhow::Error::msg)
}

/// The evaluation-window disclosure — stderr (every format) and the framing
/// block's last line (table only) render it. `None` without the period flags,
/// which is what keeps every existing output byte-identical.
fn window_line(window: TimeRange) -> Option<String> {
    (!window.is_all()).then(|| {
        format!(
            "report window: {}",
            period::display(window, JST_OFFSET_SECS)
        )
    })
}

/// The JSON `window` object — added only when the flags drew a window, so the
/// JSON contract without the flags is untouched.
fn window_json(window: TimeRange) -> Option<serde_json::Value> {
    (!window.is_all()).then(|| {
        let (since, until) = period::bounds(window, JST_OFFSET_SECS);
        serde_json::json!({ "since": since, "until": until })
    })
}

/// Attach the evaluation window to a JSON payload — only when the flags drew
/// one, so the JSON contract without the flags is untouched.
fn with_window(mut payload: serde_json::Value, window: TimeRange) -> serde_json::Value {
    if let Some(w) = window_json(window) {
        payload["window"] = w;
    }
    payload
}

/// Print the evaluation window beside the freshness line (stderr, every
/// format) — piped stdout and `--format json` stay clean.
fn eprint_window(window: TimeRange) {
    if let Some(line) = window_line(window) {
        eprintln!("{}", Style::stderr().dim(&line));
    }
}

/// A framing block with the evaluation window appended (table only — markdown
/// is the bare table, json the machine contract).
fn framed(text: &str, window: TimeRange) -> String {
    match window_line(window) {
        Some(line) => format!("{text}\n{line}"),
        None => text.to_string(),
    }
}

/// An empty report's wording when the flags drew a window: the action part
/// ("run analyze") would misread — the store may hold history outside the
/// window, and the observation stands within it. The window itself is on
/// stderr beside the freshness line.
fn empty_in_window(message: &str, window: TimeRange) -> String {
    match window.is_all() {
        true => message.to_string(),
        false => {
            let head = message.split(" — ").next().unwrap_or(message);
            format!("{head} in the evaluation window")
        }
    }
}

/// The catalog row's unused label: a windowed read means "unused *within the
/// window*" — the catalog is current config, the usage half is historical.
fn unused_label(window: TimeRange) -> &'static str {
    match window.is_all() {
        true => "UNUSED",
        false => "UNUSED (window)",
    }
}

/// One direction of a two-window comparison. `Unavailable` is 比較不能: the
/// previous side holds no events, so there is nothing to compare against —
/// labeled, never read as a drop to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Same,
    Unavailable,
}

impl Direction {
    /// The terse table label.
    fn label(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Same => "same",
            Direction::Unavailable => "n/a",
        }
    }

    /// The JSON direction value — explicit where the table is terse.
    fn json_value(self) -> &'static str {
        match self {
            Direction::Unavailable => "unavailable",
            other => other.label(),
        }
    }
}

/// Which way a metric moved between the two windows. An empty previous side
/// (0 events) is 比較不能 — the store holds no baseline for the row, so no
/// direction is claimed whatever the current value is; a delta against an
/// empty side is a coverage fact, not a move.
fn direction_of<T: PartialOrd + Default>(previous: T, current: T) -> Direction {
    if previous == T::default() {
        return Direction::Unavailable;
    }
    match current.partial_cmp(&previous) {
        Some(std::cmp::Ordering::Greater) => Direction::Up,
        Some(std::cmp::Ordering::Less) => Direction::Down,
        _ => Direction::Same,
    }
}

/// One integer metric's JSON cell: both windows' values, the delta, and the
/// direction — with the delta null when the previous side is empty, so no
/// change is claimed for a comparison that cannot be made.
fn metric_json_int(previous: i64, current: i64) -> serde_json::Value {
    let direction = direction_of(previous, current);
    let delta = match direction {
        Direction::Unavailable => serde_json::Value::Null,
        _ => serde_json::json!(current - previous),
    };
    serde_json::json!({
        "previous": previous,
        "current": current,
        "delta": delta,
        "direction": direction.json_value(),
    })
}

/// One float metric's JSON cell — the duration form of [`metric_json_int`].
fn metric_json_float(previous: f64, current: f64) -> serde_json::Value {
    let direction = direction_of(previous, current);
    let delta = match direction {
        Direction::Unavailable => serde_json::Value::Null,
        _ => serde_json::json!(current - previous),
    };
    serde_json::json!({
        "previous": previous,
        "current": current,
        "delta": delta,
        "direction": direction.json_value(),
    })
}

/// The two windows' per-kind rollups paired by kind — a kind present in only
/// one window keeps None on the other side, which reads downstream as an
/// empty (0-event) side. Sorted by kind, the aggregation's own order.
fn kind_pairs(
    agg: &TwoWindow<TrendWindowAggregates>,
) -> Vec<(String, Option<KindRollup>, Option<KindRollup>)> {
    let mut pairs: std::collections::BTreeMap<String, (Option<KindRollup>, Option<KindRollup>)> =
        std::collections::BTreeMap::new();
    for rollup in &agg.current.by_kind {
        pairs.entry(rollup.kind.clone()).or_default().0 = Some(rollup.clone());
    }
    for rollup in &agg.previous.by_kind {
        pairs.entry(rollup.kind.clone()).or_default().1 = Some(rollup.clone());
    }
    pairs
        .into_iter()
        .map(|(kind, (current, previous))| (kind, current, previous))
        .collect()
}

/// The trend's JSON contract: both windows' ranges under `window`, the
/// per-kind rows, the skill deltas, and the session/subagent totals — every
/// metric carrying both windows' values, the delta, and the direction.
fn trend_json(pair: WindowPair, agg: &TwoWindow<TrendWindowAggregates>) -> serde_json::Value {
    let (current_since, current_until) = period::bounds(pair.current, JST_OFFSET_SECS);
    let (previous_since, previous_until) = period::bounds(pair.previous, JST_OFFSET_SECS);
    let kinds: Vec<serde_json::Value> = kind_pairs(agg)
        .into_iter()
        .map(|(kind, current, previous)| {
            let (c, p) = (current.as_ref(), previous.as_ref());
            serde_json::json!({
                "kind": kind,
                "events": metric_json_int(
                    p.map_or(0, |r| r.events),
                    c.map_or(0, |r| r.events),
                ),
                "out_tokens": metric_json_int(
                    p.map_or(0, |r| r.out_tokens),
                    c.map_or(0, |r| r.out_tokens),
                ),
                "duration_sec": metric_json_float(
                    p.map_or(0.0, |r| r.duration_sec),
                    c.map_or(0.0, |r| r.duration_sec),
                ),
            })
        })
        .collect();
    let skills: Vec<serde_json::Value> = skill_deltas(&agg.current.skills, &agg.previous.skills)
        .iter()
        .map(|d| {
            let direction = direction_of(d.previous, d.current);
            let delta = match direction {
                Direction::Unavailable => serde_json::Value::Null,
                _ => serde_json::json!(d.delta),
            };
            serde_json::json!({
                "skill": d.skill,
                "previous": d.previous,
                "current": d.current,
                "delta": delta,
                "direction": direction.json_value(),
            })
        })
        .collect();
    serde_json::json!({
        "window": {
            "current": { "since": current_since, "until": current_until },
            "previous": { "since": previous_since, "until": previous_until },
        },
        "kinds": kinds,
        "skills": skills,
        "sessions": metric_json_int(agg.previous.sessions, agg.current.sessions),
        "subagent_runs": metric_json_int(
            agg.previous.subagent_runs,
            agg.current.subagent_runs,
        ),
        "sub_tokens": metric_json_int(agg.previous.sub_tokens, agg.current.sub_tokens),
    })
}

/// A table cell pair for an integer metric: the signed delta and the
/// direction — both n/a when the previous side is empty.
fn delta_cells_int(previous: i64, current: i64) -> (String, String) {
    match direction_of(previous, current) {
        Direction::Unavailable => ("n/a".to_string(), "n/a".to_string()),
        direction => (
            format!("{:+}", current - previous),
            direction.label().to_string(),
        ),
    }
}

/// A table cell pair for a float metric — the duration form of
/// [`delta_cells_int`].
fn delta_cells_float(previous: f64, current: f64) -> (String, String) {
    match direction_of(previous, current) {
        Direction::Unavailable => ("n/a".to_string(), "n/a".to_string()),
        direction => (
            format!("{:+.1}", current - previous),
            direction.label().to_string(),
        ),
    }
}

/// The per-kind section's rows: one row per kind and metric (events, output
/// tokens, seconds), so each row carries one comparison and one direction.
/// Both windows' values keep their columns even when the comparison is n/a.
fn trend_kind_rows(agg: &TwoWindow<TrendWindowAggregates>) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for (kind, current, previous) in kind_pairs(agg) {
        let (c, p) = (current.as_ref(), previous.as_ref());
        let (p_events, c_events) = (p.map_or(0, |r| r.events), c.map_or(0, |r| r.events));
        let (p_tokens, c_tokens) = (p.map_or(0, |r| r.out_tokens), c.map_or(0, |r| r.out_tokens));
        let (p_secs, c_secs) = (
            p.map_or(0.0, |r| r.duration_sec),
            c.map_or(0.0, |r| r.duration_sec),
        );
        let (delta, direction) = delta_cells_int(p_events, c_events);
        rows.push(vec![
            kind.clone(),
            "events".to_string(),
            p_events.to_string(),
            c_events.to_string(),
            delta,
            direction,
        ]);
        let (delta, direction) = delta_cells_int(p_tokens, c_tokens);
        rows.push(vec![
            kind.clone(),
            "out tokens".to_string(),
            p_tokens.to_string(),
            c_tokens.to_string(),
            delta,
            direction,
        ]);
        let (delta, direction) = delta_cells_float(p_secs, c_secs);
        rows.push(vec![
            kind,
            "seconds".to_string(),
            format!("{p_secs:.1}"),
            format!("{c_secs:.1}"),
            delta,
            direction,
        ]);
    }
    rows
}

/// The skill section's rows: `skill_deltas`' order (largest movement first),
/// one row per skill with both windows' invocation counts.
fn trend_skill_rows(agg: &TwoWindow<TrendWindowAggregates>) -> Vec<Vec<String>> {
    skill_deltas(&agg.current.skills, &agg.previous.skills)
        .iter()
        .map(|d| {
            let (delta, direction) = delta_cells_int(d.previous, d.current);
            vec![
                d.skill.clone(),
                d.previous.to_string(),
                d.current.to_string(),
                delta,
                direction,
            ]
        })
        .collect()
}

/// The totals section's rows: sessions, subagent runs, and subagent tokens
/// along the sessions axis.
fn trend_total_rows(agg: &TwoWindow<TrendWindowAggregates>) -> Vec<Vec<String>> {
    [
        ("sessions", agg.previous.sessions, agg.current.sessions),
        (
            "subagent runs",
            agg.previous.subagent_runs,
            agg.current.subagent_runs,
        ),
        (
            "subagent tokens",
            agg.previous.sub_tokens,
            agg.current.sub_tokens,
        ),
    ]
    .into_iter()
    .map(|(label, previous, current)| {
        let (delta, direction) = delta_cells_int(previous, current);
        vec![
            label.to_string(),
            previous.to_string(),
            current.to_string(),
            delta,
            direction,
        ]
    })
    .collect()
}

/// The trend preamble's text: what the report compares and how to read an
/// n/a, plus the previous window's range — the current one comes from
/// `framed`. Table format only.
fn trend_preamble(pair: WindowPair) -> String {
    format!(
        "Usage trend — this window against the equal-length window right before it.\n\
         A delta marked n/a: the previous side holds no events, so the comparison is\n\
         unavailable — not a drop. previous window: {}",
        period::display(pair.previous, JST_OFFSET_SECS)
    )
}

/// The trend's two-window disclosure — stderr beside the freshness line,
/// every format, so piped stdout and `--format json` stay clean.
fn eprint_trend_windows(pair: WindowPair) {
    let line = format!(
        "trend windows: current {} | previous {}",
        period::display(pair.current, JST_OFFSET_SECS),
        period::display(pair.previous, JST_OFFSET_SECS),
    );
    eprintln!("{}", Style::stderr().dim(&line));
}

/// A section label between a trend report's tables — printed in table and
/// markdown alike, so pasted tables keep their reading order.
fn trend_section(label: &str) {
    println!("\n{label}");
}

/// The usage trend: what each kind of event, each skill, and the session and
/// subagent totals did this window against the equal-length window right
/// before it. The pair is resolved before the store opens (`parse_pair`), so
/// a bad date fails like every other period error, and the store's numbers
/// are the same windowed queries the single-window views read.
fn trend(sess: &Session, pair: WindowPair, format: Format) -> Result<()> {
    let store = &sess.store;
    eprint_trend_windows(pair);
    let agg = store.trend_aggregates(pair)?;
    if format == Format::Json {
        return emit_json(&trend_json(pair, &agg));
    }
    // Nothing in either window *and* no session on either axis: the store
    // holds nothing to compare — unlike an empty previous window, this is
    // the analyze hint's case.
    if agg.current.by_kind.is_empty()
        && agg.previous.by_kind.is_empty()
        && agg.current.sessions == 0
        && agg.previous.sessions == 0
    {
        println!("no usage in either comparison window — run `saniwalens analyze` first");
        return Ok(());
    }
    preamble(format, &framed(&trend_preamble(pair), pair.current));
    render(
        &[
            "kind",
            "metric",
            "previous",
            "current",
            "delta",
            "direction",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
        ],
        &trend_kind_rows(&agg),
        format,
    );
    trend_section("Skills (largest movement first):");
    render(
        &["skill", "previous", "current", "delta", "direction"],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
        ],
        &trend_skill_rows(&agg),
        format,
    );
    trend_section("Totals:");
    render(
        &["total", "previous", "current", "delta", "direction"],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
        ],
        &trend_total_rows(&agg),
        format,
    );
    if agg.previous.by_kind.is_empty() {
        note(&format!(
            "the previous window ({}) holds no events in this store — comparisons\n\
             marked n/a have nothing to compare against, so no change is claimed for them",
            period::display(pair.previous, JST_OFFSET_SECS)
        ));
    }
    Ok(())
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
fn stuck(sess: &Session, window: TimeRange, format: Format) -> Result<()> {
    const GAP_SECS: i64 = 5 * 60;
    const MIN_EDITS: u32 = 4;
    let store = &sess.store;
    eprint_window(window);
    let episodes = detect_thrash(&store.file_edits(window)?, GAP_SECS, MIN_EDITS);
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
        return emit_json(&with_window(
            serde_json::json!({ "episodes": episodes }),
            window,
        ));
    }
    if episodes.is_empty() {
        println!(
            "{}",
            empty_in_window(
                &format!("no thrash episodes (>= {MIN_EDITS} rapid re-edits) — run analyze first"),
                window
            )
        );
        return Ok(());
    }
    preamble(
        format,
        &framed(
            "Files the agent kept re-editing in rapid bursts — the signature of getting stuck\n\
             and retrying. Worth checking what blocked it there (a rule? missing context?).",
            window,
        ),
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
fn failures(sess: &Session, filter: &ScopeFilter, window: TimeRange, format: Format) -> Result<()> {
    struct FrictionRow {
        project: Option<String>,
        category: String,
        count: i64,
        /// How many projects the count spreads across (global scope only).
        spread: Option<usize>,
    }

    let store = &sess.store;
    eprint_window(window);
    let cells = store.error_counts_by_project(window)?;

    let counts: Vec<FrictionRow> = match filter {
        ScopeFilter::All => store
            .error_counts(window)?
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
                        .suggestion(sess.harness.instruction_file()),
                })
            })
            .collect();
        return emit_json(&with_window(
            serde_json::json!({ "total": total, "failures": failures }),
            window,
        ));
    }
    if total == 0 {
        println!(
            "{}",
            empty_in_window(
                "no tool failures found for this scope — run `saniwalens analyze` first?",
                window
            )
        );
        return Ok(());
    }

    preamble(
        format,
        &framed(
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
                        sess.harness.global_config_ref()
                    )
                }
                ScopeFilter::Project(None) => {
                    format!(
                        "Failures each project owns (it holds the majority of that kind). Fix each\n\
                     in that project's own {}, not globally.",
                        sess.harness.project_config_ref()
                    )
                }
                ScopeFilter::Project(Some(_)) => {
                    format!(
                        "Every recurring failure inside this one project, most frequent first.\n\
                     Fix them in this project's own {}.",
                        sess.harness.project_config_ref()
                    )
                }
            },
            window,
        ),
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
                ErrorCategory::from_label(&row.category)
                    .suggestion(sess.harness.instruction_file()),
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
fn prompts(sess: &Session, window: TimeRange, format: Format) -> Result<()> {
    let store = &sess.store;
    eprint_window(window);
    let counts = store.prompt_behavior_counts(window)?;
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
        return emit_json(&with_window(
            serde_json::json!({ "total": total, "behaviors": behaviors }),
            window,
        ));
    }
    if total == 0 {
        // An analyzed store with sessions but no prompts means this harness
        // harvests no prompt signal (opencode v1 does not) — telling the user
        // to run `analyze` again would send them in a circle.
        let (sessions, _) = store.session_stats(window)?;
        if !window.is_all() {
            if sessions > 0 {
                println!(
                    "no prompts in the evaluation window — the store holds {sessions} analyzed session(s) within it"
                );
            } else {
                println!("no prompts in the evaluation window");
            }
        } else if sessions > 0 {
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
        &framed(
            &format!(
                "How you steer {}, classified per prompt: instruct (substantive asks),\n\
                 steer (\"go ahead\" / \"yes\"), correct (\"no, instead…\"), question. Lots of\n\
                 steering = room for more autonomy; lots of correcting = unclear upfront specs.",
                sess.harness.display_name()
            ),
            window,
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
fn overhead(sess: &Session, window: TimeRange, format: Format) -> Result<()> {
    let store = &sess.store;
    eprint_window(window);
    let floor = store.baseline_floor(window)?;
    let config = store.always_on_config_tokens()?;
    let residual = floor.map(|floor| (floor - config).max(0));
    let per_project = store.baseline_floor_per_project(window)?;

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
        return emit_json(&with_window(
            serde_json::json!({
                "floor": floor,
                "config_tokens": config,
                "residual": residual,
                "per_project": per_project,
            }),
            window,
        ));
    }
    // No session start observed — say so rather than substituting a mid-session
    // context that would read as a startup cost.
    let Some(floor) = floor else {
        let (sessions, _) = store.session_stats(window)?;
        if sessions == 0 {
            println!(
                "{}",
                empty_in_window("no data — run `saniwalens analyze` first", window)
            );
        } else {
            println!(
                "always-on floor unavailable — none of the {sessions} analyzed session(s)\n\
                 carries an observable session start. A store written before session-start\n\
                 tracking keeps its old rows (analyze never re-reads a closed session), so\n\
                 the floor appears as newer sessions are analyzed. Discarding the store\n\
                 would backfill it from the {} still on disk, at the cost of every\n\
                 session whose {} {} has already pruned.",
                sess.harness.raw_source(),
                sess.harness.source_noun(),
                sess.harness.display_name()
            );
        }
        return Ok(());
    };
    let residual = (floor - config).max(0);

    preamble(
        format,
        &framed(
            "What every session pays in context before any work starts, and how much of\n\
             it your own config files account for.",
            window,
        ),
    );
    let style = Style::stdout();
    println!(
        "Observed always-on floor (leanest session-start context): {}",
        style.bold(&format!("{floor} tokens"))
    );
    println!(
        "  readable config ({}): {config} tokens",
        sess.harness.readable_config()
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

fn analyze(sess: &Session, projects: Option<PathBuf>, format: Format) -> Result<()> {
    let stats = run_analyze(&sess.harness, projects, &sess.db_path)?;
    let store = &sess.store;
    // `analyze` takes the whole history — no period flags reach it.
    let (sub_tokens, sub_agents) = store.subagent_totals(TimeRange::ALL)?;
    if format == Format::Json {
        return emit_json(&serde_json::json!({
            "sessions": stats.sessions,
            "unchanged": stats.skipped,
            "skill_invocations": stats.spans_total,
            "surfaces": stats.surface_count,
            "subagent_tokens": sub_tokens,
            "subagents": sub_agents,
            "permission_denials": stats.denials,
            "db": sess.db_path.display().to_string(),
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
        sess.db_path.display()
    );
    Ok(())
}

/// Analyze raw input into the store, dispatching on the harness. The guard
/// runs first: a store built by another harness refuses the write rather than
/// mixing rows (`docs/specs/harness.md`).
fn run_analyze(harness: &Harness, projects: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    guard_harness(
        &Store::open(db, OpenMode::ReadWrite).context("open store")?,
        harness,
        db,
    )?;
    harness.analyze(projects, db)
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
///
/// Under a drawn window an empty split is usually the two time axes at work —
/// the total reads the session's stamped start, the rows read each run's own
/// epoch — so the message names the window instead of pointing at a refresh
/// that cannot fill a straddle. Without the flags the refresh hint stays.
fn warn_missing_agent_split(coverage: &SplitCoverage, window: TimeRange) {
    if let Some(message) = missing_split_warning(coverage, window) {
        eprintln!("{}", Style::stderr().warn(&message));
    }
}

/// The missing-split message, if there is one to print: the refresh hint
/// without a window, the window note under one, `None` when the rows cover
/// the total.
fn missing_split_warning(coverage: &SplitCoverage, window: TimeRange) -> Option<String> {
    if !coverage.is_unavailable() {
        return None;
    }
    if window.is_all() {
        Some(
            "store: per-agent subagent split unavailable (store predates it) \
             — run `saniwalens analyze` (or drop --frozen) to fill it in"
                .to_string(),
        )
    } else {
        Some(format!(
            "store: per-agent subagent split unavailable in this window — those \
             {} tokens over {} run(s) fall outside the report window",
            coverage.missing_tokens, coverage.missing_runs
        ))
    }
}
fn usage(sess: &Session, by: Option<&str>, window: TimeRange, format: Format) -> Result<()> {
    let store = &sess.store;
    eprint_window(window);

    if let Some(by) = by {
        let bucket = Bucket::parse(by)
            .with_context(|| format!("unknown --by value '{by}' (year|month|week|day|hour)"))?;
        return usage_by_time(store, bucket, window, format);
    }

    let skills = store.skill_usage(window)?;
    // Where the tokens actually go: main-thread skill output vs subagents. The
    // subagent figure is usually the larger by far — worth seeing before reading
    // the per-skill table.
    let main_out: i64 = skills.iter().map(|row| row.out_tokens).sum();
    let (sub_tokens, sub_agents) = store.subagent_totals(window)?;
    let agents = store.subagent_by_agent(window)?;
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
    warn_missing_agent_split(&agent_split, window);
    if format == Format::Json {
        return emit_json(&with_window(
            serde_json::json!({
                "tokens": {
                    "main_out": main_out,
                    "sub_tokens": sub_tokens,
                    "sub_agents": sub_agents,
                },
                "agent_split": agent_split,
                "agents": agents,
                "skills": skills,
            }),
            window,
        ));
    }
    // A subagent total with neither table behind it is not "nothing happened" —
    // it is a store whose per-agent split has not been extracted yet, and saying
    // "no usage" there would be false.
    if skills.is_empty() && agents.is_empty() && sub_tokens == 0 {
        println!(
            "{}",
            empty_in_window(
                "no skill usage found — run `saniwalens analyze` first",
                window
            )
        );
        return Ok(());
    }
    preamble(
        format,
        &framed(
            "What ran and what it cost. First the subagent bill per agent type\n\
             (costliest first), then how often each skill ran — output tokens written,\n\
             context growth while it ran, and wall-clock seconds (most-invoked first).\n\
             A section with nothing to show is omitted.",
            window,
        ),
    );
    println!(
        "tokens: main-thread skill output {main_out}, subagents {sub_tokens} ({sub_agents} agents)"
    );
    // Stated between the total and the table, so the table below is read as
    // the part it is. Under a drawn window the gap is the straddle, so the
    // note names the window rather than the pruned transcripts.
    if let Some(note) = agent_split.partial_note_in(window) {
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
fn usage_by_time(store: &Store, bucket: Bucket, window: TimeRange, format: Format) -> Result<()> {
    let events = store.skill_event_costs(window)?;

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
        return emit_json(&with_window(
            serde_json::json!({ "buckets": buckets }),
            window,
        ));
    }
    if events.is_empty() {
        println!(
            "{}",
            empty_in_window(
                "no skill usage found — run `saniwalens analyze` first",
                window
            )
        );
        return Ok(());
    }

    preamble(
        format,
        &framed(
            "Skill activity per time bucket (times in JST). A long-running skill counts\n\
             whole in the bucket it started in, so fine buckets can show spikes.",
            window,
        ),
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

/// Join the catalogued surfaces against usage: each installed surface with its
/// static cost and (for usage-measurable kinds) invocation count, unused ones
/// flagged. Usage with no matching surface is shown as orphaned.
fn inventory(
    sess: &Session,
    filter: &ScopeFilter,
    window: TimeRange,
    format: Format,
) -> Result<()> {
    let store = &sess.store;
    eprint_window(window);
    let catalog: Vec<_> = store
        .effective_catalog(window)?
        .into_iter()
        .filter(|e| in_scope(filter, &e.scope, &e.project))
        .collect();
    let usage = store.usage_counts(window)?;

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
                    "status": if measurable && entry.uses == 0 {
                        if window.is_all() { "unused" } else { "unused-in-window" }
                    } else if !measurable { "catalog-only" } else { "" },
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
        return emit_json(&with_window(
            serde_json::json!({ "surfaces": items }),
            window,
        ));
    }
    if catalog.is_empty() && usage.is_empty() {
        println!("nothing catalogued — run `saniwalens analyze` first");
        return Ok(());
    }

    preamble(format, &framed(sess.harness.inventory_preamble(), window));
    let mut rows: Vec<Vec<String>> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        // "unused" is only meaningful for usage-measurable kinds; catalog-only
        // kinds (rules, hooks, CLAUDE.md) emit no events, so 0 means nothing.
        let (uses_cell, status) = if measurable {
            (
                entry.uses.to_string(),
                if entry.uses == 0 {
                    unused_label(window)
                } else {
                    ""
                },
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
fn waste(sess: &Session, filter: &ScopeFilter, window: TimeRange, format: Format) -> Result<()> {
    let store = &sess.store;
    eprint_window(window);
    let catalog: Vec<_> = store
        .effective_catalog(window)?
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
        return emit_json(&with_window(
            serde_json::json!({ "wedges": wedges }),
            window,
        ));
    }
    if found.is_empty() {
        println!(
            "{}",
            empty_in_window(
                "nothing wasteful found — run `saniwalens analyze` first",
                window
            )
        );
        return Ok(());
    }

    preamble(
        format,
        &framed(
            "Concrete cleanup opportunities in your config, ranked by what removing each\n\
             one actually saves at session start. A \"~\" figure is declutter-only — just\n\
             the description is loaded at startup; the body costs nothing until invoked.",
            window,
        ),
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
        let lines = doctor_agent_lines(&f, "saniwalens usage", TimeRange::ALL);
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
        let lines = doctor_agent_lines(&f, "saniwalens usage", TimeRange::ALL);
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
        let lines = doctor_agent_lines(&f, "saniwalens usage", TimeRange::ALL);
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
    fn doctor_labels_a_windowed_partial_split_with_the_window() {
        // Under a drawn window the gap is the straddle — the total reads the
        // session's stamped start, the rows read each run's own epoch — so the
        // doctor names the window instead of the pruned transcripts.
        let f = findings_with_agents(
            3_093_466,
            375,
            vec![
                agent("general-purpose", 11, 315_663),
                agent("codex:codex-rescue", 14, 62_067),
            ],
        );
        let window = TimeRange {
            start: Some(1_790_780_400),
            end: Some(1_791_385_199),
        };
        let lines = doctor_agent_lines(&f, "saniwalens usage", window);
        assert!(
            lines[0].contains("fall outside the report window"),
            "{}",
            lines[0]
        );
        assert!(
            !lines[0].contains("analyzed before runs were extracted"),
            "{}",
            lines[0]
        );
        // Without a window the pruned-transcript wording stays, byte for byte.
        let lines = doctor_agent_lines(&f, "saniwalens usage", TimeRange::ALL);
        assert!(
            lines[0].contains("analyzed before runs were extracted"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn doctor_names_no_agents_when_the_split_is_unavailable() {
        // The gap is reported on stderr with its refresh hint; there is
        // nothing to list in-band.
        let f = findings_with_agents(3_093_466, 375, vec![]);
        assert!(doctor_agent_lines(&f, "saniwalens usage", TimeRange::ALL).is_empty());
    }

    #[test]
    fn a_windowed_unavailable_split_names_the_window_not_a_refresh() {
        // A total with no rows behind it under a drawn window is the straddle,
        // not a store that predates the split — `analyze` (or dropping
        // --frozen) cannot fill it, so the message names the window instead.
        let f = findings_with_agents(3_093_466, 375, vec![]);
        let window = TimeRange {
            start: Some(1_790_780_400),
            end: Some(1_791_385_199),
        };
        let warning = missing_split_warning(&f.agent_split, window).unwrap();
        assert!(
            warning.contains("fall outside the report window"),
            "{warning}"
        );
        assert!(!warning.contains("run `saniwalens analyze`"), "{warning}");

        // Without the flags the refresh hint stays, byte for byte.
        assert_eq!(
            missing_split_warning(&f.agent_split, TimeRange::ALL).unwrap(),
            "store: per-agent subagent split unavailable (store predates it) \
             — run `saniwalens analyze` (or drop --frozen) to fill it in"
        );
        // A split the rows cover warns about nothing.
        let complete = findings_with_agents(100, 1, vec![agent("a", 1, 100)]);
        assert_eq!(
            missing_split_warning(&complete.agent_split, TimeRange::ALL),
            None
        );
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
    fn parse_profile_defaults_to_none() {
        // --profile is optional and global, like --sess.harness.
        let cli = Cli::try_parse_from(["saniwalens", "doctor"]).unwrap();
        assert_eq!(cli.profile, None);
        let cli = Cli::try_parse_from(["saniwalens", "--profile", "work", "doctor"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
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
            ("claude", &[] as &[&str], optimize_mod::RunMode::Interactive)
        );
        assert_eq!(
            launch_command(&Harness::Opencode),
            (
                "opencode",
                &["run", "--format", "json", "--title", "saniwalens optimize"] as &[&str],
                optimize_mod::RunMode::Headless
            )
        );
        assert_eq!(
            launch_command(&Harness::Kilocode),
            (
                "kilo",
                &["--prompt"] as &[&str],
                optimize_mod::RunMode::Interactive
            )
        );
    }

    // ---- PBI 06: the delivery seam ----

    /// A delivery double: records what would have been launched instead of
    /// spawning anything, so the briefing's argv can be asserted against a
    /// mock (BDD: "従来と同一の引数で claude が起動する").
    #[cfg(unix)]
    struct RecordingAdvisor {
        direct: Vec<(String, Vec<String>, String)>,
        headless: Vec<(String, Vec<String>, String)>,
    }

    #[cfg(unix)]
    fn success_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    #[cfg(unix)]
    impl RecordingAdvisor {
        fn new() -> Self {
            Self {
                direct: Vec::new(),
                headless: Vec::new(),
            }
        }
    }

    #[cfg(unix)]
    impl AdvisorLaunch for RecordingAdvisor {
        fn direct(
            &mut self,
            binary: &str,
            leading: &[&str],
            prompt: &str,
        ) -> Result<std::process::ExitStatus> {
            self.direct.push((
                binary.to_string(),
                leading.iter().map(|s| s.to_string()).collect(),
                prompt.to_string(),
            ));
            Ok(success_status())
        }

        fn headless(
            &mut self,
            binary: &str,
            leading: &[&str],
            prompt: &str,
        ) -> Result<(std::process::ExitStatus, PathBuf)> {
            self.headless.push((
                binary.to_string(),
                leading.iter().map(|s| s.to_string()).collect(),
                prompt.to_string(),
            ));
            Ok((success_status(), PathBuf::from("/tmp/never-opened.log")))
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_print_delivery_writes_to_stdout_and_launches_nothing() {
        let plan = DeliveryPlan::print("FULL PROMPT".to_string());
        let mut advisor = RecordingAdvisor::new();

        deliver(plan, &mut advisor).unwrap();

        assert!(advisor.direct.is_empty(), "print never spawns");
        assert!(advisor.headless.is_empty(), "print never spawns");
    }

    #[cfg(unix)]
    #[test]
    fn the_interactive_delivery_hands_the_double_the_measured_launch() {
        use std::cell::RefCell;
        let seen_path: RefCell<Option<PathBuf>> = RefCell::new(None);
        let plan = DeliveryPlan::launch(
            "claude",
            &[],
            DeliveryMode::Interactive,
            "the rendered briefing".to_string(),
            "Claude Code",
            false,
            |path| {
                *seen_path.borrow_mut() = Some(path.to_path_buf());
                "POINTER PROMPT".to_string()
            },
        );
        let mut advisor = RecordingAdvisor::new();

        deliver(plan, &mut advisor).unwrap();

        assert_eq!(advisor.direct.len(), 1);
        assert_eq!(advisor.direct[0].0, "claude");
        assert!(advisor.direct[0].1.is_empty());
        assert_eq!(advisor.direct[0].2, "POINTER PROMPT");
        // The private pointer file is removed the moment the run ends.
        let path = seen_path.borrow().clone().expect("pointer got the path");
        assert!(!path.exists(), "briefing file outlived its run");
    }

    #[cfg(unix)]
    #[test]
    fn headless_routes_through_the_logged_adapter_unless_verbose() {
        let headless_plan = DeliveryPlan::launch(
            "opencode",
            &["run", "--format", "json"],
            DeliveryMode::Headless,
            "briefing".to_string(),
            "opencode",
            false,
            |_| "pointer".to_string(),
        );
        let mut advisor = RecordingAdvisor::new();
        deliver(headless_plan, &mut advisor).unwrap();
        assert_eq!(advisor.headless.len(), 1, "quiet headless logs its stream");
        assert!(advisor.direct.is_empty());

        // `--verbose` inherits stdio for the raw streaming instead
        // (`docs/specs/harness.md`).
        let verbose_plan = DeliveryPlan::launch(
            "opencode",
            &["run", "--format", "json"],
            DeliveryMode::Headless,
            "briefing".to_string(),
            "opencode",
            true,
            |_| "pointer".to_string(),
        );
        let mut advisor = RecordingAdvisor::new();
        deliver(verbose_plan, &mut advisor).unwrap();
        assert_eq!(advisor.direct.len(), 1);
        assert!(advisor.headless.is_empty());
    }

    #[test]
    fn the_delivery_mode_is_the_flag_over_the_launch_shape() {
        assert_eq!(
            delivery_mode(true, optimize_mod::RunMode::Interactive),
            DeliveryMode::Print
        );
        assert_eq!(
            delivery_mode(false, optimize_mod::RunMode::Interactive),
            DeliveryMode::Interactive
        );
        assert_eq!(
            delivery_mode(false, optimize_mod::RunMode::Headless),
            DeliveryMode::Headless
        );
    }

    // ---- PBI 2026-10-30-01: the period flags ----

    #[test]
    fn the_period_flags_parse_on_every_report_view() {
        // The flags exist on all nine views and nothing else.
        let views = [
            vec!["usage"],
            vec!["trend"],
            vec!["inventory"],
            vec!["waste"],
            vec!["overhead"],
            vec!["prompts"],
            vec!["failures"],
            vec!["stuck"],
            vec!["doctor"],
        ];
        for view in views {
            let mut argv = vec!["saniwalens"];
            argv.extend(view.iter().copied());
            argv.extend(["--since", "2026-10-01", "--until", "2026-10-07"]);
            Cli::try_parse_from(argv).expect("the view takes the period flags");
        }
        // The other commands do not.
        for argv in [
            vec!["saniwalens", "analyze", "--since", "2026-10-01"],
            vec!["saniwalens", "sql", "--last", "7d"],
            vec!["saniwalens", "optimize", "--since", "2026-10-01"],
        ] {
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "the period flags stay on the report views"
            );
        }
    }

    #[test]
    fn last_conflicts_with_the_explicit_dates() {
        let err = Cli::try_parse_from([
            "saniwalens",
            "usage",
            "--last",
            "7d",
            "--since",
            "2026-10-01",
        ])
        .err()
        .expect("the flags are rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = Cli::try_parse_from([
            "saniwalens",
            "waste",
            "--last",
            "7d",
            "--until",
            "2026-10-07",
        ])
        .err()
        .expect("the flags are rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        // Alone, `--last` parses.
        let cli = Cli::try_parse_from(["saniwalens", "usage", "--last", "7d"]).unwrap();
        let Command::Usage {
            by,
            since,
            until,
            last,
            format,
            frozen,
            db,
        } = cli.command
        else {
            panic!("the command is `usage`");
        };
        assert_eq!(by, None);
        assert_eq!(since, None);
        assert_eq!(until, None);
        assert_eq!(last.as_deref(), Some("7d"));
        assert_eq!(format, None);
        assert!(!frozen);
        assert_eq!(db, None);
    }

    #[test]
    fn the_window_helpers_say_nothing_without_the_flags() {
        let all = TimeRange::ALL;
        assert_eq!(window_line(all), None);
        assert_eq!(window_json(all), None);
        assert!(framed("text", all) == "text");
        assert_eq!(
            empty_in_window("no skill usage found — run `saniwalens analyze` first", all),
            "no skill usage found — run `saniwalens analyze` first"
        );
        assert_eq!(unused_label(all), "UNUSED");
    }

    #[test]
    fn the_window_helpers_disclose_a_drawn_window() {
        let window = TimeRange {
            start: Some(1_790_780_400),
            end: Some(1_791_385_199),
        };
        assert_eq!(
            window_line(window).as_deref(),
            Some("report window: since 2026-10-01T00:00:00+09:00, until 2026-10-07T23:59:59+09:00")
        );
        assert_eq!(
            window_json(window),
            Some(serde_json::json!({
                "since": "2026-10-01T00:00:00+09:00",
                "until": "2026-10-07T23:59:59+09:00",
            }))
        );
        // The framing block appends the line to the text; the JSON payload
        // gains the `window` key without touching the rest.
        assert!(framed("text", window).contains("text\nreport window: since"));
        let payload = with_window(serde_json::json!({ "total": 3 }), window);
        assert_eq!(payload["window"]["since"], "2026-10-01T00:00:00+09:00");
        assert_eq!(payload["total"], 3);
    }

    #[test]
    fn an_empty_report_reads_within_the_window() {
        let window = TimeRange {
            start: Some(1),
            end: Some(2),
        };
        assert_eq!(
            empty_in_window(
                "no skill usage found — run `saniwalens analyze` first",
                window
            ),
            "no skill usage found in the evaluation window"
        );
        assert_eq!(
            empty_in_window("no data — run `saniwalens analyze` first", window),
            "no data in the evaluation window"
        );
        assert_eq!(unused_label(window), "UNUSED (window)");
    }

    // ---- PBI 2026-10-30-02 part 2: the trend view's CLI wiring ----

    use crate::core::period::WindowPair;
    use crate::core::span::{SessionStart, Source, Span};
    use crate::store::{IngestBatch, SessionMeta};

    fn trend_session(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            project: "demo".to_string(),
            slug: "demo".to_string(),
            root: String::new(),
            source_path: format!("/tmp/{id}.jsonl"),
            sub_tokens: 0,
            sub_agent_count: 0,
            start: None,
        }
    }

    fn trend_span(skill: &str, epoch_secs: i64, out_tokens: u64) -> Span {
        Span {
            skill: skill.to_string(),
            source: Source::Slash,
            started_epoch_ms: epoch_secs * 1000,
            duration_sec: 2.0,
            out_tokens,
            ctx_growth: 5,
            ctx_start: 0,
            ctx_peak: 5,
            model: None,
            is_trailing: false,
            spawns: Vec::new(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    /// Usage in both comparison windows: previous [1_000, 2_000] (two skill
    /// spans, one file_edit), current [2_001, 3_000] (one skill span, one
    /// file_edit). Each window's spans ride its own session — the stamped
    /// start is the earliest of the declared start and the spans, so a
    /// session's own spans must not reach into the other window.
    fn two_window_store() -> Store {
        let mut store = Store::in_memory().unwrap();
        let mut past = trend_session("past");
        past.start = Some(SessionStart {
            timestamp_ms: 1_500 * 1000,
            ctx: 9_000,
        });
        past.sub_tokens = 400;
        past.sub_agent_count = 1;
        let past_spans = [
            trend_span("commit", 1_500, 100),
            trend_span("review", 1_800, 50),
        ];
        store
            .ingest(IngestBatch::new(&past, &past_spans, &[]).work_events(&[(
                1_900_000,
                "file_edit",
                "main.rs",
                None,
            )]))
            .unwrap();

        let mut present = trend_session("present");
        present.start = Some(SessionStart {
            timestamp_ms: 2_500 * 1000,
            ctx: 10_000,
        });
        present.sub_tokens = 500;
        present.sub_agent_count = 2;
        let present_spans = [trend_span("commit", 2_500, 300)];
        store
            .ingest(
                IngestBatch::new(&present, &present_spans, &[]).work_events(&[(
                    2_600_000,
                    "file_edit",
                    "main.rs",
                    None,
                )]),
            )
            .unwrap();
        store
    }

    /// A store with usage only in the current window: the previous side of
    /// every comparison is empty.
    fn one_sided_store() -> Store {
        let mut store = Store::in_memory().unwrap();
        let mut present = trend_session("present");
        present.start = Some(SessionStart {
            timestamp_ms: 2_500 * 1000,
            ctx: 10_000,
        });
        present.sub_tokens = 500;
        present.sub_agent_count = 2;
        let present_spans = [trend_span("commit", 2_500, 300)];
        store
            .ingest(
                IngestBatch::new(&present, &present_spans, &[]).work_events(&[(
                    2_600_000,
                    "file_edit",
                    "main.rs",
                    None,
                )]),
            )
            .unwrap();
        store
    }

    fn trend_pair() -> WindowPair {
        WindowPair {
            current: TimeRange {
                start: Some(2_001),
                end: Some(3_000),
            },
            previous: TimeRange {
                start: Some(1_000),
                end: Some(2_000),
            },
        }
    }

    #[test]
    fn the_trend_direction_judgment() {
        use Direction::{Down, Same, Unavailable, Up};
        assert_eq!(direction_of(3.0, 5.0), Up);
        assert_eq!(direction_of(5.0, 3.0), Down);
        assert_eq!(direction_of(5.0, 5.0), Same);
        // An empty previous side is 比較不能: the comparison is unavailable
        // whatever the current value is — never a -100% drop to zero.
        assert_eq!(direction_of(0.0, 5.0), Unavailable);
        assert_eq!(direction_of(0.0, 0.0), Unavailable);
        // Integer sides share the judgment.
        assert_eq!(direction_of(4, 1), Down);
        assert_eq!(direction_of(1, 4), Up);
        assert_eq!(direction_of(7, 7), Same);
        assert_eq!(direction_of(0, 7), Unavailable);
    }

    #[test]
    fn trend_resolves_a_rolling_pair_through_the_flag_surface() {
        // No flags: 7d against the 7d right before it, the two tiling the
        // timeline with no shared second.
        let cli = Cli::try_parse_from(["saniwalens", "trend"]).unwrap();
        let Command::Trend {
            since, until, last, ..
        } = cli.command
        else {
            panic!("expected the trend command");
        };
        let pair = parse_pair(since.as_deref(), until.as_deref(), last.as_deref()).unwrap();
        let day = 86_400;
        assert_eq!(
            pair.current.end.unwrap() - pair.current.start.unwrap() + 1,
            7 * day
        );
        assert_eq!(
            pair.previous.end.unwrap() - pair.previous.start.unwrap() + 1,
            7 * day
        );
        assert_eq!(pair.previous.end.unwrap() + 1, pair.current.start.unwrap());

        // --last 24h: the same shape at 24h (86_400 seconds).
        let cli = Cli::try_parse_from(["saniwalens", "trend", "--last", "24h"]).unwrap();
        let Command::Trend { last, .. } = cli.command else {
            panic!("expected the trend command");
        };
        let pair = parse_pair(None, None, last.as_deref()).unwrap();
        assert_eq!(
            pair.current.end.unwrap() - pair.current.start.unwrap() + 1,
            24 * 3_600
        );
        assert_eq!(pair.previous.end.unwrap() + 1, pair.current.start.unwrap());
    }

    #[test]
    fn trend_period_errors_fail_before_the_store_opens() {
        // The dispatch resolves the pair before Session::open_read, so these
        // fail as period errors — non-zero, before any store work or
        // freshness line. The clock is read inside parse_pair; none of the
        // errors depend on it.
        let cases: [(&[&str], &str); 4] = [
            (
                &["saniwalens", "trend", "--since", "2026/10/01"],
                "invalid --since '2026/10/01'",
            ),
            (
                &["saniwalens", "trend", "--until", "2026-10-01"],
                "--until requires --since",
            ),
            (
                &[
                    "saniwalens",
                    "trend",
                    "--since",
                    "2026-10-07",
                    "--until",
                    "2026-10-01",
                ],
                "empty window: --since 2026-10-07 is after --until 2026-10-01",
            ),
            // The span parses (it fits i64) but its doubled window does not —
            // the pair resolution's overflow guard must reject it as the
            // invalid-span error, not panic.
            (
                &["saniwalens", "trend", "--last", "92233720368547d"],
                "invalid --last '92233720368547d'",
            ),
        ];
        for (args, expected) in cases {
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Trend {
                since, until, last, ..
            } = cli.command
            else {
                panic!("expected the trend command for {args:?}");
            };
            let err = parse_pair(since.as_deref(), until.as_deref(), last.as_deref())
                .expect_err("a bad period must fail");
            assert!(
                err.to_string().contains(expected),
                "expected {expected:?} in {err:?}"
            );
        }
    }

    #[test]
    fn trend_json_carries_both_windows_and_their_deltas() {
        let store = two_window_store();
        let pair = trend_pair();
        let agg = store.trend_aggregates(pair).unwrap();
        let json = trend_json(pair, &agg);

        // Both windows' ranges are disclosed under `window`.
        assert_eq!(
            json["window"]["current"]["since"],
            "1970-01-01T09:33:21+09:00"
        );
        assert_eq!(
            json["window"]["current"]["until"],
            "1970-01-01T09:50:00+09:00"
        );
        assert_eq!(
            json["window"]["previous"]["since"],
            "1970-01-01T09:16:40+09:00"
        );
        assert_eq!(
            json["window"]["previous"]["until"],
            "1970-01-01T09:33:20+09:00"
        );

        // Per-kind rows carry both windows' values and the delta. The skill
        // invocations went 2 -> 1 (down); their tokens 150 -> 300 (up).
        let kinds = json["kinds"].as_array().unwrap();
        let skill = kinds
            .iter()
            .find(|k| k["kind"] == "skill_invocation")
            .unwrap();
        assert_eq!(skill["events"]["previous"], 2);
        assert_eq!(skill["events"]["current"], 1);
        assert_eq!(skill["events"]["delta"], -1);
        assert_eq!(skill["events"]["direction"], "down");
        assert_eq!(skill["out_tokens"]["previous"], 150);
        assert_eq!(skill["out_tokens"]["current"], 300);
        assert_eq!(skill["out_tokens"]["delta"], 150);
        assert_eq!(skill["out_tokens"]["direction"], "up");
        // A kind both windows share at the same size reads as unchanged.
        let edit = kinds.iter().find(|k| k["kind"] == "file_edit").unwrap();
        assert_eq!(edit["events"]["previous"], 1);
        assert_eq!(edit["events"]["current"], 1);
        assert_eq!(edit["events"]["delta"], 0);
        assert_eq!(edit["events"]["direction"], "same");

        // Skills are ordered by movement: review (0 <- 1) before commit (1, 1).
        let skills = json["skills"].as_array().unwrap();
        assert_eq!(skills[0]["skill"], "review");
        assert_eq!(skills[0]["previous"], 1);
        assert_eq!(skills[0]["current"], 0);
        assert_eq!(skills[0]["delta"], -1);
        assert_eq!(skills[0]["direction"], "down");
        assert_eq!(skills[1]["skill"], "commit");
        assert_eq!(skills[1]["direction"], "same");

        // Session and subagent totals follow the sessions axis.
        assert_eq!(json["sessions"]["previous"], 1);
        assert_eq!(json["sessions"]["current"], 1);
        assert_eq!(json["sessions"]["direction"], "same");
        assert_eq!(json["subagent_runs"]["previous"], 1);
        assert_eq!(json["subagent_runs"]["current"], 2);
        assert_eq!(json["subagent_runs"]["delta"], 1);
        assert_eq!(json["subagent_runs"]["direction"], "up");
        assert_eq!(json["sub_tokens"]["previous"], 400);
        assert_eq!(json["sub_tokens"]["current"], 500);
        assert_eq!(json["sub_tokens"]["delta"], 100);
        assert_eq!(json["sub_tokens"]["direction"], "up");
    }

    #[test]
    fn trend_marks_comparisons_unavailable_when_the_previous_window_holds_no_events() {
        let store = one_sided_store();
        let pair = trend_pair();
        let agg = store.trend_aggregates(pair).unwrap();
        let json = trend_json(pair, &agg);

        // Every metric cell is labeled unavailable, with no delta claimed —
        // the previous side is empty, not a real zero baseline.
        for kind in json["kinds"].as_array().unwrap() {
            for metric in ["events", "out_tokens", "duration_sec"] {
                assert_eq!(
                    kind[metric]["direction"], "unavailable",
                    "unexpected direction for {} of {}",
                    metric, kind["kind"]
                );
                assert_eq!(kind[metric]["delta"], serde_json::Value::Null);
            }
        }
        for skill in json["skills"].as_array().unwrap() {
            assert_eq!(skill["direction"], "unavailable");
            assert_eq!(skill["delta"], serde_json::Value::Null);
        }
        for total in ["sessions", "subagent_runs", "sub_tokens"] {
            assert_eq!(json[total]["direction"], "unavailable");
            assert_eq!(json[total]["delta"], serde_json::Value::Null);
        }

        // And nothing in the payload reads as a drop to zero.
        let text = serde_json::to_string(&json).unwrap();
        assert!(!text.contains("-100%"));
        assert!(!text.contains("dropped"));
    }

    #[test]
    fn trend_rows_label_unavailable_comparisons_instead_of_drops() {
        let store = one_sided_store();
        let agg = store.trend_aggregates(trend_pair()).unwrap();

        // The kind rows keep both windows' values (the previous side renders
        // as 0) but mark the delta and direction n/a.
        let rows = trend_kind_rows(&agg);
        assert!(
            rows.iter()
                .any(|r| r[1] == "events" && r[2] == "0" && r[3] == "1"),
            "the current window's values still show: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| r[4] == "n/a" && r[5] == "n/a"),
            "every comparison is n/a: {rows:?}"
        );

        // No percentage and no drop claim anywhere in the rendered cells.
        let flat = rows
            .iter()
            .flat_map(|r| r.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!flat.contains('%'), "no percentages: {flat:?}");
        assert!(!flat.contains("-100"), "no -100: {flat:?}");
    }

    #[test]
    fn the_trend_preamble_discloses_both_windows() {
        let pair = trend_pair();
        let text = trend_preamble(pair);
        assert!(
            text.contains("previous window: since 1970-01-01T09:16:40+09:00"),
            "the previous window is named in the preamble: {text:?}"
        );
        // framed() appends the current window — both windows end up in the
        // table's framing block.
        let framed_text = framed(&text, pair.current);
        assert!(
            framed_text.contains("report window: since 1970-01-01T09:33:21+09:00"),
            "the current window is disclosed too: {framed_text:?}"
        );
    }

    #[test]
    fn trend_runs_end_to_end_against_a_session() {
        // The full view path — including the empty store's early message —
        // runs against a real Session without error. The payload and row
        // structures are asserted by the tests above.
        let two = Session {
            harness: Harness::Claude,
            db_path: PathBuf::new(),
            store: two_window_store(),
        };
        trend(&two, trend_pair(), Format::Table).unwrap();
        trend(&two, trend_pair(), Format::Markdown).unwrap();
        trend(&two, trend_pair(), Format::Json).unwrap();
        let empty = Session {
            harness: Harness::Claude,
            db_path: PathBuf::new(),
            store: Store::in_memory().unwrap(),
        };
        trend(&empty, trend_pair(), Format::Table).unwrap();
        trend(&empty, trend_pair(), Format::Json).unwrap();
    }
}
