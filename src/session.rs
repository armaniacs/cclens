//! One invocation's worth of resolution: config file, flags, and defaults into
//! a harness plus a store path, then the opened and guarded store.
//!
//! `run()` arms used to repeat this wiring eleven times over: every command
//! re-derived the same triple through free functions scattered across
//! `cli.rs`. The last three real defects lived in that wiring, not in parsing
//! or rendering, so it gets one module with one seam. See
//! `docs/specs/cli.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::adapter::config_file;
use crate::adapter::harness::Harness;
use crate::store::Store;

/// The environment `Session` resolution reads. The caller supplies both
/// roots; tests pass temp dirs, production passes the live environment.
#[derive(Debug)]
pub struct SessionEnv<'a> {
    pub state_home: Option<&'a Path>,
    pub home: Option<&'a str>,
    pub config_home: Option<&'a Path>,
}

/// What `--harness` / `--profile` plus the config file resolve to, before any
/// store is opened.
#[derive(Debug, PartialEq)]
pub struct ResolvedCli {
    pub harness: Harness,
    pub config_db: Option<PathBuf>,
}

/// Resolve one invocation's harness and default store from flags plus the
/// tool's own config file. Pure over the supplied environment: tests pass
/// temp dirs, production passes the live roots.
pub fn resolve_cli(
    cli_harness: Option<&str>,
    cli_profile: Option<&str>,
    env: &SessionEnv,
) -> Result<ResolvedCli> {
    let config_path = config_file_path(env.config_home, env.home);
    let config_dir = config_path.as_deref().and_then(|p| p.parent());
    let file = config_path
        .as_deref()
        .map(config_file::load)
        .transpose()?
        .flatten();
    let resolved = config_file::resolve(cli_harness, cli_profile, None, file.as_ref(), config_dir)?;
    Ok(ResolvedCli {
        harness: parse_harness(resolved.harness.as_deref())?,
        config_db: resolved.db,
    })
}

/// Parse `--harness`. Absent means Claude Code — every pre-flag invocation
/// keeps its behavior, which is what keeps the flag upstream-mergeable.
pub fn parse_harness(value: Option<&str>) -> Result<Harness> {
    match value {
        None => Ok(Harness::Claude),
        Some(v) => Harness::parse(v)
            .with_context(|| format!("unknown --harness '{v}' (claude | opencode | kilocode)")),
    }
}

/// Where the tool's own config file lives, if anywhere: under
/// `$XDG_CONFIG_HOME/saniwalens/`, else `~/.config/saniwalens/`. Pure over
/// its inputs like `store_path`, for the same reason. `None` means there is
/// no file to read, which means "behave exactly as before".
pub fn config_file_path(config_home: Option<&Path>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = config_home.filter(|dir| dir.is_absolute()) {
        return Some(dir.join("saniwalens/config.toml"));
    }
    home.filter(|dir| Path::new(dir).is_absolute())
        .map(|dir| Path::new(dir).join(".config/saniwalens/config.toml"))
}

/// Counters from one analyze run, for the command's summary line and the
/// freshness header on auto-refreshing reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalyzeStats {
    pub sessions: usize,
    pub skipped: usize,
    pub spans_total: usize,
    pub surface_count: usize,
    pub denials: usize,
}

/// One invocation's resolved world: which harness, which store file, and the
/// opened, harness-guarded store. Every command builds exactly this and
/// nothing else before doing its own work.
pub struct Session {
    pub harness: Harness,
    pub db_path: PathBuf,
    pub store: Store,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The store holds an open SQLite connection, which is deliberately
        // left out: Debug is for the tests that unwrap through Session.
        f.debug_struct("Session")
            .field("harness", &self.harness)
            .field("db_path", &self.db_path)
            .finish()
    }
}

impl Session {
    /// Resolve the path, move a legacy store into place on a pure default,
    /// open, and guard. The analyze arms run analysis themselves afterwards
    /// (which guards again); the read arms go through [`Session::open_read`].
    pub fn open(
        harness: &Harness,
        db: Option<PathBuf>,
        config_db: Option<&Path>,
        env: &SessionEnv,
    ) -> Result<Self> {
        let path = Self::resolve_path(harness, db, config_db, env)?;
        Self::open_resolved(harness, path)
    }

    /// Open and guard an already-resolved path. Split from [`Session::open`]
    /// so a caller can resolve the path, parse its own flags, and only then
    /// open — the exact evaluation order `run()`'s optimize arm had before the
    /// Session seam (`docs/specs/cli.md`).
    pub(crate) fn open_resolved(harness: &Harness, path: PathBuf) -> Result<Self> {
        let store = Store::open(&path).context("open store")?;
        guard_harness(&store, harness, &path)?;
        Ok(Session {
            harness: *harness,
            db_path: path,
            store,
        })
    }

    /// The read path for the views: open, then analyze unless frozen — exactly
    /// what `open_for_read` did, messages included. `analyze` runs the refresh
    /// analysis; tests pass a stub.
    pub fn open_read(
        harness: &Harness,
        db: Option<PathBuf>,
        config_db: Option<&Path>,
        frozen: bool,
        env: &SessionEnv,
        analyze: impl FnOnce(&Harness, Option<PathBuf>, &Path) -> Result<AnalyzeStats>,
    ) -> Result<Self> {
        if frozen {
            let sess = Self::open(harness, db, config_db, env)?;
            eprint_freshness(&freshness_line(sess.store.meta("analyzed_at")?.as_deref()));
            return Ok(sess);
        }
        let mut sess = Self::open(harness, db, config_db, env)?;
        sess.refresh(analyze)?;
        Ok(sess)
    }

    /// The read-only path for `sql`: resolve and migrate like everything
    /// else, but open read-only and never refresh. The freshness line stays
    /// with the caller, which prints it only when the store carries one.
    pub fn open_readonly(
        harness: &Harness,
        db: Option<PathBuf>,
        config_db: Option<&Path>,
        env: &SessionEnv,
    ) -> Result<Self> {
        let pure_default = db.is_none() && config_db.is_none();
        let explicit = db.or_else(|| config_db.map(Path::to_path_buf));
        let path = store_path(explicit, harness, env.state_home, env.home.map(Path::new))?;
        if pure_default {
            migrate_legacy_store(&path);
        }
        let store = Store::open_readonly(&path)
            .with_context(|| format!("open store {} (run `analyze` first?)", path.display()))?;
        guard_harness(&store, harness, &path)?;
        Ok(Session {
            harness: *harness,
            db_path: path,
            store,
        })
    }

    /// Re-analyze against the roots the store was built from (recorded in
    /// meta); a fresh db falls back to the defaults.
    fn refresh(
        &mut self,
        analyze: impl FnOnce(&Harness, Option<PathBuf>, &Path) -> Result<AnalyzeStats>,
    ) -> Result<()> {
        let projects = match self.db_path.exists() {
            true => Store::open(&self.db_path)
                .context("open store")?
                .meta("projects_dir")?
                .map(PathBuf::from),
            false => None,
        };
        let stats = analyze(&self.harness, projects, &self.db_path)?;
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "store: refreshed just now ({} {} re-analyzed, {} unchanged)",
                stats.sessions,
                self.harness.input_unit(),
                stats.skipped
            ))
        );
        self.store = Store::open(&self.db_path).context("open store")?;
        Ok(())
    }

    pub(crate) fn resolve_path(
        harness: &Harness,
        db: Option<PathBuf>,
        config_db: Option<&Path>,
        env: &SessionEnv,
    ) -> Result<PathBuf> {
        let pure_default = db.is_none() && config_db.is_none();
        let path = store_path(
            db.or_else(|| config_db.map(Path::to_path_buf)),
            harness,
            env.state_home,
            env.home.map(Path::new),
        )?;
        // The legacy-store move only applies to the pure default resolution:
        // an explicit `--db` or a configured one is the user's own choice.
        if pure_default {
            migrate_legacy_store(&path);
        }
        Ok(path)
    }
}

/// Refuse to open a store another harness built. A store with no `harness`
/// key predates the flag and is adopted (the Claude default), never refused.
pub(crate) fn guard_harness(store: &Store, harness: &Harness, db: &Path) -> Result<()> {
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

/// Print a freshness line to stderr — dimmed context normally, highlighted
/// when it carries a staleness hint the user should act on.
pub(crate) fn eprint_freshness(line: &str) {
    let style = Style::stderr();
    if line.contains("saniwalens analyze") {
        eprintln!("{}", style.warn(line));
    } else {
        eprintln!("{}", style.dim(line));
    }
}

/// The `--frozen` freshness header: how old the store is, with a refresh hint
/// once it is older than a day. This is what lets the user *notice* staleness.
pub(crate) fn freshness_line(analyzed_at: Option<&str>) -> String {
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

/// ANSI styling for the human formats. Colors turn on only when the target is
/// a terminal, `NO_COLOR` is unset, and `TERM` is not `dumb` — so pipes,
/// captures, and `--format markdown|json` always get plain bytes.
pub(crate) struct Style {
    on: bool,
}

impl Style {
    pub(crate) fn stdout() -> Self {
        use std::io::IsTerminal;
        Self {
            on: use_color(std::io::stdout().is_terminal()),
        }
    }

    pub(crate) fn stderr() -> Self {
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

    pub(crate) fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub(crate) fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Section headings (`== … ==`, table headers).
    pub(crate) fn heading(&self, text: &str) -> String {
        self.paint("1;36", text)
    }

    /// Pointers to follow-up commands.
    pub(crate) fn command(&self, text: &str) -> String {
        self.paint("36", text)
    }

    /// Attention: a finding's count, a wedge label, a staleness warning.
    pub(crate) fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub(crate) fn good(&self, text: &str) -> String {
        self.paint("32", text)
    }
}

fn use_color(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_dir(sub: &str) -> PathBuf {
        std::env::temp_dir().join(format!("saniwalens-session-{sub}-{}", std::process::id()))
    }

    #[test]
    fn parse_harness_defaults_to_claude_and_rejects_the_rest() {
        assert_eq!(parse_harness(None).unwrap(), Harness::Claude);
        assert_eq!(parse_harness(Some("opencode")).unwrap(), Harness::Opencode);
        assert!(parse_harness(Some("auto")).is_err());
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
        assert_eq!(
            config_file_path(Some(Path::new("relative/config")), None),
            None
        );
    }

    #[test]
    fn config_file_path_without_a_home_is_none() {
        assert_eq!(config_file_path(None, None), None);
    }

    #[test]
    fn resolve_cli_without_a_file_means_builtins() {
        let dir = example_dir("no-file");
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: Some(&dir),
        };
        let resolved = resolve_cli(None, None, &env).unwrap();
        assert_eq!(
            resolved,
            ResolvedCli {
                harness: Harness::Claude,
                config_db: None
            }
        );
    }

    #[test]
    fn resolve_cli_reads_default_harness_and_config_relative_db() {
        let dir = example_dir("file-defaults");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join("saniwalens");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.toml"),
            "default_harness = \"kilocode\"\ndb = \"k.db\"\n",
        )
        .unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: Some(&dir),
        };
        let resolved = resolve_cli(None, None, &env).unwrap();
        assert_eq!(resolved.harness, Harness::Kilocode);
        assert_eq!(resolved.config_db, Some(cfg.join("k.db")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_cli_flags_beat_the_file() {
        let dir = example_dir("file-override");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join("saniwalens");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.toml"),
            "default_harness = \"kilocode\"\n[profiles.work]\nharness = \"claude\"\n",
        )
        .unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: Some(&dir),
        };
        let resolved = resolve_cli(Some("opencode"), Some("work"), &env).unwrap();
        assert_eq!(resolved.harness, Harness::Opencode);
        assert_eq!(resolved.config_db, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_cli_unknown_profile_lists_available() {
        let dir = example_dir("file-unknown-profile");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join("saniwalens");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.toml"),
            "[profiles.work]\nharness = \"claude\"\n",
        )
        .unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: Some(&dir),
        };
        let err = resolve_cli(None, Some("play"), &env).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("play"), "{msg}");
        assert!(msg.contains("work"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_cli_unknown_harness_keeps_the_flag_error() {
        let dir = example_dir("file-bad-harness");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join("saniwalens");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.toml"), "default_harness = \"auto\"\n").unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: Some(&dir),
        };
        let err = resolve_cli(None, None, &env).unwrap_err();
        assert!(format!("{err:#}").contains("unknown --harness 'auto'"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn store_with_harness(dir: &Path, file: &str, label: Option<&str>) -> PathBuf {
        let path = dir.join(file);
        let mut store = Store::open(&path).unwrap();
        if let Some(label) = label {
            store.set_meta("harness", label).unwrap();
        }
        path
    }

    fn panic_analyzer() -> impl FnOnce(&Harness, Option<PathBuf>, &Path) -> Result<AnalyzeStats> {
        |_, _, _| panic!("analyzer must not run on this path")
    }

    #[test]
    fn open_resolves_config_db_and_migrates_legacy() {
        // The default resolution moves the pre-rename directory into place,
        // exactly like the old resolve_db did.
        let dir = example_dir("open-migrate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cclens")).unwrap();
        std::fs::write(dir.join("cclens/cclens.db"), b"rows").unwrap();
        std::fs::write(dir.join("file.db"), b"").unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let from_config = dir.join("file.db");
        let sess = Session::open(&Harness::Claude, None, Some(&from_config), &env).unwrap();
        assert_eq!(sess.db_path, from_config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_prefers_explicit_db_and_skips_migration() {
        let dir = example_dir("open-explicit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cclens")).unwrap();
        std::fs::write(dir.join("cclens/cclens.db"), b"legacy").unwrap();
        let chosen = dir.join("chosen.db");
        std::fs::write(&chosen, b"").unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let sess = Session::open(&Harness::Claude, Some(chosen.clone()), None, &env).unwrap();
        assert_eq!(sess.db_path, chosen);
        assert!(dir.join("cclens/cclens.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_guards_a_wrong_harness_store() {
        let dir = example_dir("open-guard");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_with_harness(&dir, "s.db", Some(Harness::Opencode.label()));
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let err = Session::open(&Harness::Claude, Some(path), None, &env).unwrap_err();
        assert!(format!("{err:#}").contains("--harness opencode"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_adopts_a_store_without_a_harness_key() {
        let dir = example_dir("open-adopt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_with_harness(&dir, "s.db", None);
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let sess = Session::open(&Harness::Claude, Some(path.clone()), None, &env).unwrap();
        assert_eq!(sess.db_path, path);
        assert_eq!(sess.harness, Harness::Claude);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_read_frozen_never_runs_the_analyzer() {
        let dir = example_dir("open-frozen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_with_harness(&dir, "s.db", Some(Harness::Claude.label()));
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let sess = Session::open_read(
            &Harness::Claude,
            Some(path),
            None,
            true,
            &env,
            panic_analyzer(),
        )
        .unwrap();
        assert_eq!(sess.harness, Harness::Claude);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_read_refresh_runs_the_analyzer_with_meta_projects() {
        use std::cell::RefCell;
        let dir = example_dir("open-refresh");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_with_harness(&dir, "s.db", Some(Harness::Claude.label()));
        {
            let mut store = Store::open(&path).unwrap();
            store
                .set_meta("projects_dir", "/tmp/example/projects")
                .unwrap();
        }
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let stats = AnalyzeStats {
            sessions: 3,
            skipped: 1,
            spans_total: 0,
            surface_count: 0,
            denials: 0,
        };
        let sess = Session::open_read(
            &Harness::Claude,
            Some(path.clone()),
            None,
            false,
            &env,
            |h, projects, db| {
                seen.borrow_mut().push(format!(
                    "{}|{}|{}",
                    h.label(),
                    projects
                        .map(|p| p.display().to_string())
                        .as_deref()
                        .unwrap_or("-"),
                    db.display()
                ));
                Ok(AnalyzeStats { ..stats })
            },
        )
        .unwrap();
        assert_eq!(sess.db_path, path);
        assert_eq!(
            seen.borrow().as_slice(),
            &[format!("claude|/tmp/example/projects|{}", path.display())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_read_refresh_without_projects_passes_none() {
        use std::cell::RefCell;
        let dir = example_dir("open-refresh-none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_with_harness(&dir, "s.db", Some(Harness::Claude.label()));
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let stats = AnalyzeStats {
            sessions: 0,
            skipped: 0,
            spans_total: 0,
            surface_count: 0,
            denials: 0,
        };
        Session::open_read(
            &Harness::Claude,
            Some(path),
            None,
            false,
            &env,
            |h, projects, _| {
                seen.borrow_mut()
                    .push(format!("{}|{}", h.label(), projects.is_some()));
                Ok(stats)
            },
        )
        .unwrap();
        assert_eq!(seen.borrow().as_slice(), &["claude|false".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_read_frozen_wrong_harness_is_refused() {
        let dir = example_dir("open-frozen-wrong");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_with_harness(&dir, "s.db", Some(Harness::Opencode.label()));
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let err = Session::open_read(
            &Harness::Claude,
            Some(path),
            None,
            true,
            &env,
            panic_analyzer(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("--harness opencode"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_readonly_missing_file_errors_like_before() {
        let dir = example_dir("open-readonly-missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env = SessionEnv {
            state_home: Some(&dir),
            home: None,
            config_home: None,
        };
        let err = Session::open_readonly(&Harness::Claude, Some(dir.join("nope.db")), None, &env)
            .unwrap_err();
        assert!(format!("{err:#}").contains("run `analyze` first?"));
        let _ = std::fs::remove_dir_all(&dir);
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
}
