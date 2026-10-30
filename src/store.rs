//! SQLite store: persist extracted spans as events and query them back. This
//! layer knows SQL but nothing about raw Claude Code formats. See
//! `docs/specs/storage.md`.
//!
//! The schema tracks the spec's `events` table; `attrs_json` is the one column
//! deferred until a report needs it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite::params_from_iter;

use crate::core::period::TimeRange;
use crate::core::span::{SessionStart, Source, Span};
use crate::core::subagent::SubagentRun;
use crate::core::surface::Surface;
use crate::core::thrash::FileEdit;
use crate::core::usage::UsageEvent;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    project         TEXT NOT NULL,
    slug            TEXT NOT NULL,
    root            TEXT NOT NULL DEFAULT '',
    source_path     TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    sub_tokens      INTEGER NOT NULL DEFAULT 0,
    sub_agent_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY,
    session_id    TEXT NOT NULL,
    source_path   TEXT NOT NULL,
    source_line   INTEGER,
    kind          TEXT NOT NULL,
    surface_kind  TEXT,
    surface_id    TEXT,
    source        TEXT,
    started_at    TEXT NOT NULL,
    started_epoch INTEGER NOT NULL,
    duration_sec  REAL NOT NULL,
    out_tokens    INTEGER NOT NULL,
    ctx_growth    INTEGER NOT NULL,
    ctx_start     INTEGER NOT NULL,
    ctx_peak      INTEGER NOT NULL,
    model         TEXT,
    target        TEXT,
    sub_tokens           INTEGER NOT NULL DEFAULT 0,
    sub_agent_count      INTEGER NOT NULL DEFAULT 0,
    sub_tokens_estimated INTEGER NOT NULL DEFAULT 0,
    is_trailing          INTEGER NOT NULL DEFAULT 0
);
-- One row per subagent execution: which agent type ran, and what its own output
-- cost. The session total says how much subagent work cost; this says *whose*.
-- `source_path` is the parent session's transcript — the ingest delete key, as
-- for events — while `run_path` points at the subagent's own transcript.
CREATE TABLE IF NOT EXISTS subagent_runs (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT NOT NULL,
    source_path       TEXT NOT NULL,
    run_path          TEXT NOT NULL,
    agent             TEXT,
    agent_id          TEXT NOT NULL,
    tool_use_id       TEXT,
    root_tool_use_id  TEXT,
    parent_agent_id   TEXT,
    spawn_depth       INTEGER NOT NULL DEFAULT 1,
    model             TEXT,
    prompt_id         TEXT,
    out_tokens        INTEGER NOT NULL DEFAULT 0,
    started_at        TEXT NOT NULL,
    started_epoch     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS surfaces (
    kind          TEXT NOT NULL,
    id            TEXT NOT NULL,
    scope         TEXT NOT NULL,
    project       TEXT NOT NULL DEFAULT '',
    config_path   TEXT,
    static_tokens INTEGER,
    startup_tokens INTEGER,
    load_mode     TEXT NOT NULL,
    PRIMARY KEY (kind, id, scope, project)
);
-- Analyze-run metadata (analyzed_at, projects_dir, config_dir) so read
-- commands can report freshness and re-run the analysis with the same roots,
-- plus analyzer_version, which invalidates the ingest fingerprints above.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Incremental-ingest fingerprints: a transcript whose (mtime, size) matches is
-- skipped on re-analyze (see docs/specs/storage.md).
CREATE TABLE IF NOT EXISTS ingested_files (
    path  TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size  INTEGER NOT NULL
);
";

/// Indexes are declared apart from the tables so `migrate` can apply them only
/// after reconciling columns. Declared alongside the table, an index over a
/// column a future release adds would be replayed against a legacy shape, fail on
/// the missing column, and take the whole migration down with it.
const INDEX_SCHEMA: &str = "
CREATE INDEX events_by_surface ON events(surface_kind, surface_id);
CREATE INDEX subagent_runs_by_agent ON subagent_runs(agent);
CREATE INDEX subagent_runs_by_source ON subagent_runs(source_path);
";

/// A kind-specific meaning an event kind carries in a generic `events` column.
/// Its `Display` is the name such a meaning gets when projected (the
/// `tool_errors` view's aliases), so the table reads as the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventRole {
    /// `tool_error`: the friction category (`edit-precondition`, …).
    Category,
    /// `tool_error`: a short excerpt of the error text.
    Excerpt,
    /// `tool_error`: the tool that produced the failure.
    OriginatingTool,
    /// `tool_error`: the failed call's subject (file path / command).
    FailureTarget,
    /// `file_edit`: the basename hotspot rankings group on.
    FileBasename,
    /// `file_edit`: the edited file's full path.
    FilePath,
    /// `bash_cmd`: the command's leading word.
    CommandWord,
    /// `prompt`: the prompt's behavioral class.
    PromptBehavior,
    /// `agent_spawn`: the spawn call's id, joining `subagent_runs`.
    SpawnCallId,
}

impl std::fmt::Display for EventRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EventRole::Category => "category",
            EventRole::Excerpt => "excerpt",
            EventRole::OriginatingTool => "tool",
            EventRole::FailureTarget => "target",
            EventRole::FileBasename => "basename",
            EventRole::FilePath => "path",
            EventRole::CommandWord => "word",
            EventRole::PromptBehavior => "behavior",
            EventRole::SpawnCallId => "call_id",
        })
    }
}

/// A generic `events` column that can carry a kind-specific meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventColumn {
    SurfaceId,
    Source,
    Model,
    Target,
}

impl EventColumn {
    pub fn name(&self) -> &'static str {
        match self {
            EventColumn::SurfaceId => "surface_id",
            EventColumn::Source => "source",
            EventColumn::Model => "model",
            EventColumn::Target => "target",
        }
    }
}

impl std::fmt::Display for EventColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// **The single place** a kind's overloaded-column meaning is defined
/// (`docs/specs/storage.md`). The `tool_errors` view is generated from this
/// table and the readers take their column names from it, so a new event kind
/// is one row here — never an edit spread across view SQL and queries.
///
/// Kinds whose columns carry only their primary meaning (a `skill_invocation`'s
/// `surface_id` *is* the joined skill) need no entry. The writers fill the
/// physical columns without consulting the table — a `work_events` statement
/// spans several kinds at once — and their mapping is pinned against the table
/// by the round-trip tests (`tool_errors_view_exposes_named_columns_and_project`,
/// `file_edits_carry_their_project_session_and_full_path`, …).
pub const EVENT_ENCODING: &[(&str, &[(EventRole, EventColumn)])] = &[
    (
        "tool_error",
        &[
            (EventRole::Category, EventColumn::SurfaceId),
            (EventRole::Excerpt, EventColumn::Source),
            (EventRole::OriginatingTool, EventColumn::Model),
            (EventRole::FailureTarget, EventColumn::Target),
        ],
    ),
    (
        "file_edit",
        &[
            (EventRole::FileBasename, EventColumn::SurfaceId),
            (EventRole::FilePath, EventColumn::Target),
        ],
    ),
    (
        "bash_cmd",
        &[(EventRole::CommandWord, EventColumn::SurfaceId)],
    ),
    (
        "prompt",
        &[(EventRole::PromptBehavior, EventColumn::Source)],
    ),
    (
        "agent_spawn",
        &[(EventRole::SpawnCallId, EventColumn::Target)],
    ),
];

/// The column a `(kind, role)` meaning rides in. Panics on an entry missing
/// from [`EVENT_ENCODING`] — every caller's pair is pinned by the tests below,
/// so a panic here is a programming bug, not input.
pub fn encoding_column(kind: &str, role: EventRole) -> &'static str {
    let (_, roles) = EVENT_ENCODING
        .iter()
        .find(|(k, _)| *k == kind)
        .unwrap_or_else(|| panic!("event kind {kind:?} is not in EVENT_ENCODING"));
    roles
        .iter()
        .find(|(r, _)| *r == role)
        .unwrap_or_else(|| panic!("{kind} carries no {role} in EVENT_ENCODING"))
        .1
        .name()
}

/// The `tool_errors` view, generated from the encoding table's `tool_error`
/// entry — the view and the table that defines the meaning cannot drift, and a
/// kind added to the table leaves this definition untouched.
fn tool_errors_view_sql(encoding: &[(&str, &[(EventRole, EventColumn)])]) -> String {
    let (_, roles) = encoding
        .iter()
        .find(|(k, _)| *k == "tool_error")
        .expect("the tool_errors view is defined by its tool_error entry");
    // One projection per role, padded so the generated text matches what a
    // store built before this table already holds — no view churn on open.
    let projected: Vec<String> = roles
        .iter()
        .map(|(role, column)| format!("e.{column:<18} AS {role},"))
        .collect();
    format!(
        "-- A clean read view over tool_error events: the friction columns are
-- overloaded onto generic event columns (defined once in EVENT_ENCODING,
-- which this view is generated from), so the view names them and joins
-- the project — an ad-hoc SQL query asks for any slice without knowing
-- the encoding. `project LIKE '%--wt%'` distinguishes a worktree from the main checkout.
CREATE VIEW tool_errors AS
SELECT e.session_id        AS session_id,
       s.project           AS project,
       {}
       e.started_epoch      AS started_epoch
FROM events e JOIN sessions s ON e.session_id = s.id
WHERE e.kind = 'tool_error';",
        projected.join("\n       ")
    )
}

/// Views are pure definitions over the tables, declared apart from them for the
/// same reason as indexes plus one of their own: `CREATE VIEW IF NOT EXISTS`
/// would leave an older store's stale definition in place, and a reader would
/// silently query yesterday's column mapping. The tool_error view is generated
/// from [`EVENT_ENCODING`] so the definition and its meaning stay one thing.
fn view_schema() -> String {
    tool_errors_view_sql(EVENT_ENCODING)
}

/// Stamped into `PRAGMA user_version`. Bumped only for a change `migrate` cannot
/// reconcile — an existing column removed, retyped, or given a new meaning.
/// Additive drift (a new column, table, or view) needs no bump: `migrate`
/// converges any older store onto the declared schema in place.
const SCHEMA_VERSION: i64 = 1;

/// Tables holding analysis history. Claude Code prunes transcripts on its own
/// retention schedule, after which these rows are the only surviving record of
/// those sessions — so a shape change adds columns in place, never rebuilds.
const MIGRATED_TABLES: [&str; 4] = ["sessions", "events", "ingested_files", "meta"];

/// Tables regenerated wholesale from live config on every analyze
/// (`replace_surfaces`), so a shape change costs nothing to drop and recreate.
const REBUILT_TABLES: [&str; 1] = ["surfaces"];

/// Identity and provenance of one analyzed session.
pub struct SessionMeta {
    pub id: String,
    pub project: String,
    pub slug: String,
    /// The real directory the session started in (from the transcript's `cwd`,
    /// worktree folded) — empty when no record carried one. This is what
    /// project-config scanning walks; the slug is too lossy to reconstruct it.
    pub root: String,
    pub source_path: String,
    /// Total output tokens across this session's subagent transcripts, and how
    /// many subagents it spawned.
    pub sub_tokens: i64,
    pub sub_agent_count: i64,
    /// When this session started and the context it started with, when the
    /// transcript carried an assistant record to read it from — the only honest
    /// always-on-floor observation (`core::span::session_start`). Stored as a
    /// `session_start` event so `overhead` can distinguish "no observation" from
    /// a fabricated zero.
    pub start: Option<SessionStart>,
}

/// One row of the per-skill usage rollup. Subagent cost is deliberately not
/// rolled up per skill: under the flat-span model a skill's window can absorb
/// subagents spawned by later same-window work, so a per-skill figure
/// over-counts for skills that do not themselves spawn agents. The exact figure
/// is the session-level total (`subagent_totals`). See `docs/specs/events.md`.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct SkillUsage {
    pub skill: String,
    pub invocations: i64,
    pub out_tokens: i64,
    pub ctx_growth: i64,
    pub duration_sec: f64,
}

/// One catalogued surface row with its **effective** usage: the invocations
/// attributed to this row under per-project shadowing (a project row absorbs
/// its own project's events; the global row keeps everyone else's). See
/// `docs/specs/surfaces.md`.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct CatalogEntry {
    pub kind: String,
    pub id: String,
    /// `global` | `project`.
    pub scope: String,
    /// The owning project's normalized slug; empty for global rows.
    pub project: String,
    /// The whole definition's weight — for a skill or agent, what its body
    /// costs when invoked, not what having it installed costs.
    pub static_tokens: Option<i64>,
    /// What this surface adds to every session's startup context; `None` when
    /// unknown (an MCP server's tool schema).
    pub startup_tokens: Option<i64>,
    pub load_mode: String,
    pub uses: i64,
}

/// What one agent type cost: how many times it ran and the output tokens those
/// runs wrote. `agent` is `None` for runs whose type could not be read.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct AgentCost {
    pub agent: Option<String>,
    pub runs: i64,
    pub out_tokens: i64,
}

/// One skill event's cost, with its UTC start, for time bucketing.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct EventCost {
    pub started_epoch: i64,
    pub out_tokens: i64,
    pub ctx_growth: i64,
    pub duration_sec: f64,
}

/// What opening a store is allowed to do — the guarantees that used to be
/// spread across `open` / `open_readonly` names are now one open path with a
/// typed mode (`docs/specs/storage.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Create the file if needed, narrow it to owner-only, and migrate the
    /// schema in place. Every command but `sql`.
    ReadWrite,
    /// Open an existing file only: never create, never chmod, never migrate.
    /// The `sql` command's ad-hoc query must not mutate the derived store, so
    /// an old store's missing column surfaces as SQLite's own error.
    ReadOnly,
}

pub struct Store {
    conn: Connection,
}

/// Everything one transcript yields, submitted as a single unit
/// (`Store::ingest`). The store applies it inside one transaction in its own
/// order, so the sequence the old five functions demanded of callers — the
/// session row first, always — is unreachable rather than remembered
/// (`docs/specs/storage.md`).
pub struct IngestBatch<'a> {
    pub session: &'a SessionMeta,
    pub spans: &'a [Span],
    pub usage: &'a [UsageEvent],
    pub prompts: &'a [(usize, i64, &'a str)],
    pub subagent_runs: &'a [SubagentRun],
    pub tool_errors: &'a [(i64, &'a str, &'a str, &'a str, &'a str)],
    pub work_events: &'a [(i64, &'a str, &'a str, Option<&'a str>)],
}

impl<'a> IngestBatch<'a> {
    /// The batch's floor: the session row plus its spans and usage events.
    /// Every child kind starts empty and is added by its builder method.
    pub fn new(session: &'a SessionMeta, spans: &'a [Span], usage: &'a [UsageEvent]) -> Self {
        Self {
            session,
            spans,
            usage,
            prompts: &[],
            subagent_runs: &[],
            tool_errors: &[],
            work_events: &[],
        }
    }

    pub fn prompts(mut self, prompts: &'a [(usize, i64, &'a str)]) -> Self {
        self.prompts = prompts;
        self
    }

    pub fn subagent_runs(mut self, runs: &'a [SubagentRun]) -> Self {
        self.subagent_runs = runs;
        self
    }

    pub fn tool_errors(mut self, errors: &'a [(i64, &'a str, &'a str, &'a str, &'a str)]) -> Self {
        self.tool_errors = errors;
        self
    }

    pub fn work_events(mut self, events: &'a [(i64, &'a str, &'a str, Option<&'a str>)]) -> Self {
        self.work_events = events;
        self
    }
}

/// `create_dir_all` with owner-only permissions on every directory it creates —
/// the mode the XDG Base Directory spec prescribes for a directory a program
/// creates on the user's behalf. Directories that already exist keep theirs.
fn create_private_dir_all(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Make sure the store file is readable only by its owner: create it `0600`
/// when absent, and narrow it when it is already there with group/other bits
/// set. Creating it here rather than letting SQLite create it under the umask
/// is what removes the window — SQLite would otherwise write the schema into a
/// world-readable file and only then have it chmod'ed — and it lets SQLite
/// inherit the mode for the rollback journal it writes beside the database.
///
/// An existing store is narrowed rather than left alone: a store an older
/// saniwalens created under the umask is the realistic case, and nobody
/// deliberately shares this store. Only a regular file is touched, so pointing
/// `--db` at a directory fails at SQLite instead of silently re-moding it, and
/// a store owned by someone else is left as it is — re-moding it is not this
/// process's call, and refusing to run over it would break a store legitimately
/// created under another account.
fn ensure_owner_only(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::{OpenOptions, Permissions};
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("create store {}", path.display())),
        }
        let meta = std::fs::metadata(path)
            .with_context(|| format!("read store permissions {}", path.display()))?;
        let mode = meta.permissions().mode();
        if meta.is_file() && mode & 0o077 != 0 {
            // 0600 outright rather than masking the existing mode: masking would
            // carry over a stray execute bit, and could leave the owner without
            // the read/write the store needs to be opened at all.
            match std::fs::set_permissions(path, Permissions::from_mode(0o600)) {
                Ok(()) => {}
                // Not our file to re-mode. Everything else is a real failure.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("restrict store permissions {}", path.display()));
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The SQL fragment (a leading ` AND `) and epoch binds that narrow `column`
/// to `range`. Both ends are inclusive — `--until` counts the whole day it
/// names — and an unbounded range renders nothing, so a query's text and its
/// results are unchanged without the period flags. **The one place** the
/// report views' window becomes SQL: queries assemble it into their WHERE
/// rather than growing per-query branches.
fn range_clause(range: TimeRange, column: &str) -> (String, Vec<i64>) {
    let mut clauses = Vec::new();
    let mut binds = Vec::new();
    if let Some(start) = range.start {
        clauses.push(format!("{column} >= ?"));
        binds.push(start);
    }
    if let Some(end) = range.end {
        clauses.push(format!("{column} <= ?"));
        binds.push(end);
    }
    if clauses.is_empty() {
        (String::new(), binds)
    } else {
        (format!(" AND {}", clauses.join(" AND ")), binds)
    }
}

/// The same window for a uniform RFC 3339 UTC timestamp column
/// (`sessions.started_at`): lexicographic order is chronological order for
/// the one format the writer stamps (`epoch_ms_to_rfc3339`), and the upper
/// bound is the *exclusive* next second, so a millisecond fraction inside the
/// final second of the named day stays in. A session the writer could not
/// stamp (empty timestamp, no observable start) is **excluded** from any
/// windowed figure rather than guessed into it.
fn range_clause_utc_text(range: TimeRange, column: &str) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut binds = Vec::new();
    if let Some(start) = range.start {
        clauses.push(format!("{column} >= ?"));
        binds.push(epoch_to_rfc3339_utc(start));
    }
    if let Some(end) = range.end {
        clauses.push(format!("{column} < ?"));
        binds.push(epoch_to_rfc3339_utc(end + 1));
    }
    if clauses.is_empty() {
        (String::new(), binds)
    } else {
        clauses.push(format!("{column} <> ''"));
        (format!(" AND {}", clauses.join(" AND ")), binds)
    }
}

/// The RFC 3339 UTC text of an epoch second, in the one format
/// `sessions.started_at` is stamped with.
fn epoch_to_rfc3339_utc(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

impl Store {
    /// Open the store at `path` under a typed guarantee ([`OpenMode`]): the one
    /// open path, with what it may do — create, chmod, migrate — spelled by the
    /// mode argument rather than by which of several constructors was called.
    ///
    /// In `ReadWrite` mode the directory and file are **owner-only**: the store
    /// aggregates transcript-derived data — file paths, error excerpts, project
    /// names — for every project at one well-known user-level path, so the umask
    /// default (commonly a group/world-readable `0755` directory and `0644`
    /// file) would expose it to other local accounts.
    pub fn open(path: impl AsRef<std::path::Path>, mode: OpenMode) -> Result<Self> {
        match mode {
            OpenMode::ReadWrite => {
                let path = path.as_ref();
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    create_private_dir_all(parent)
                        .with_context(|| format!("create store directory {}", parent.display()))?;
                }
                ensure_owner_only(path)?;
                Self::from_connection(Connection::open(path)?)
            }
            OpenMode::ReadOnly => {
                // No create, no chmod, no migration: an ad-hoc query must never
                // mutate the derived store (and an absent db is an error, not a
                // fresh empty one).
                let conn = Connection::open_with_flags(
                    path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                Ok(Self { conn })
            }
        }
    }

    /// An ephemeral in-memory store, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Run an arbitrary read query, returning `(column names, rows)` with every
    /// cell stringified. Powers the `sql` command — the store's own query
    /// surface, so the analyzed data is reachable for any slice without
    /// re-parsing transcripts. Opened read-only by the caller; a non-read
    /// statement simply errors at SQLite.
    pub fn query(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let mut stmt = self.conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let ncol = columns.len();
        let rows = stmt
            .query_map([], |row| {
                (0..ncol)
                    .map(|i| Ok(value_to_string(row.get_ref(i)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((columns, rows))
    }

    /// Like `query`, but with SQLite's types preserved as JSON values — the
    /// machine half of the output contract (`--format json`): integers and
    /// reals stay numbers, NULL stays null, so a consumer never re-parses
    /// strings.
    pub fn query_json(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
        let mut stmt = self.conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let ncol = columns.len();
        let rows = stmt
            .query_map([], |row| {
                (0..ncol)
                    .map(|i| Ok(value_to_json(row.get_ref(i)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((columns, rows))
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Ingest one transcript's extraction as a single unit: one transaction,
    /// applied in the store's own order (session row → replace the file's rows
    /// → child rows → spawn settle). The old five-function API left that order
    /// to the caller's convention — `ingest_session` had to run first or its
    /// delete silently dropped the children — so the order now lives inside
    /// this one entry point (`docs/specs/storage.md`).
    pub fn ingest(&mut self, batch: IngestBatch<'_>) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::write_session(&tx, batch.session, batch.spans, batch.usage)?;
        Self::write_prompts(&tx, batch.session, batch.prompts)?;
        Self::write_subagent_runs(&tx, batch.session, batch.subagent_runs)?;
        Self::write_tool_errors(&tx, batch.session, batch.tool_errors)?;
        Self::write_work_events(&tx, batch.session, batch.work_events)?;
        tx.commit()?;
        Ok(())
    }

    /// The session row plus the replace of everything this file put into
    /// `events` — the delete-then-insert that makes re-ingest "replace, not
    /// append" (`docs/specs/storage.md`), owned by the batch so no caller can
    /// run it in the wrong order. `spans` are skill executions with cost;
    /// `usage` are point events (agent spawns, MCP tool calls) counted for the
    /// catalog join.
    fn write_session(
        tx: &rusqlite::Transaction<'_>,
        session: &SessionMeta,
        spans: &[Span],
        usage: &[UsageEvent],
    ) -> Result<()> {
        // The session's first assistant record precedes any span, so it is the
        // better start when present; the earliest span is the fallback for a
        // transcript that carried no assistant record.
        let started_epoch_ms = session
            .start
            .map(|start| start.timestamp_ms)
            .into_iter()
            .chain(spans.iter().map(|span| span.started_epoch_ms))
            .min();
        let started_at = started_epoch_ms
            .map(epoch_ms_to_rfc3339)
            .unwrap_or_default();

        tx.execute(
            "INSERT OR REPLACE INTO sessions
               (id, project, slug, root, source_path, started_at, sub_tokens, sub_agent_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                &session.id,
                &session.project,
                &session.slug,
                &session.root,
                &session.source_path,
                &started_at,
                session.sub_tokens,
                session.sub_agent_count,
            ),
        )?;
        tx.execute(
            "DELETE FROM events WHERE source_path = ?1",
            (&session.source_path,),
        )?;
        // One `session_start` event per session that carried an observable
        // start context — the always-on floor `overhead` reports. It is stamped
        // with when the session began, not left at the epoch: `sql` exposes raw
        // event rows, and a 1970 timestamp there is worse than no row at all.
        if let Some(start) = session.start {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, started_at, started_epoch,
                    duration_sec, out_tokens, ctx_growth, ctx_start, ctx_peak)
                 VALUES (?1, ?2, 'session_start', ?3, ?4, 0, 0, 0, ?5, ?5)",
                (
                    &session.id,
                    &session.source_path,
                    epoch_ms_to_rfc3339(start.timestamp_ms),
                    start.timestamp_ms / 1000,
                    start.ctx,
                ),
            )?;
        }
        for span in spans {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_kind, surface_id, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak, model,
                    sub_tokens, sub_agent_count, sub_tokens_estimated, is_trailing)
                 VALUES (?1, ?2, 'skill_invocation', 'skill', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                (
                    &session.id,
                    &session.source_path,
                    &span.skill,
                    source_label(span.source),
                    epoch_ms_to_rfc3339(span.started_epoch_ms),
                    span.started_epoch_ms / 1000,
                    span.duration_sec,
                    span.out_tokens,
                    span.ctx_growth,
                    span.ctx_start,
                    span.ctx_peak,
                    &span.model,
                    span.sub_tokens,
                    span.sub_agent_count,
                    span.sub_tokens_estimated as i64,
                    span.is_trailing as i64,
                ),
            )?;
        }
        for event in usage {
            let kind = if event.surface_kind == "agent" {
                "agent_spawn"
            } else {
                "tool_use"
            };
            // An agent spawn keeps its call id in `target` — the join key to the
            // subagent run it started (`write_subagent_runs`).
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_kind, surface_id, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak, model, target)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, 0, 0, 0, 0, 0, NULL, ?8)",
                (
                    &session.id,
                    &session.source_path,
                    kind,
                    &event.surface_kind,
                    &event.surface_id,
                    epoch_ms_to_rfc3339(event.started_epoch_ms),
                    event.started_epoch_ms / 1000,
                    &event.tool_use_id,
                ),
            )?;
        }
        Ok(())
    }

    /// Prompt-pointer events for a session: `(source_line, epoch_ms)` per user
    /// prompt. The text is not stored — only the pointer (`source_path` with
    /// `source_line`) so it can be re-read later (`docs/specs/storage.md`).
    /// Runs inside the batch transaction, after [`Self::write_session`] has
    /// replaced the file's prior rows.
    fn write_prompts(
        tx: &rusqlite::Transaction<'_>,
        session: &SessionMeta,
        prompts: &[(usize, i64, &str)],
    ) -> Result<()> {
        // The prompt's behavioral class rides in the column EVENT_ENCODING
        // names for `prompt`.
        let behavior_column = encoding_column("prompt", EventRole::PromptBehavior);
        let insert = format!(
            "INSERT INTO events
               (session_id, source_path, source_line, kind, {behavior_column},
                started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                ctx_start, ctx_peak)
             VALUES (?1, ?2, ?3, 'prompt', ?4, '', ?5, 0, 0, 0, 0, 0)"
        );
        for (line_no, epoch_ms, behavior) in prompts {
            tx.execute(
                &insert,
                (
                    &session.id,
                    &session.source_path,
                    *line_no as i64,
                    *behavior,
                    epoch_ms / 1000,
                ),
            )?;
        }
        Ok(())
    }

    /// A session's subagent runs, and the settle of the cost of the spawns that
    /// started them — inside the batch transaction, after [`Self::write_session`]
    /// inserted those spawn events (the order the old API left to its callers).
    ///
    /// Two rows describe one subagent from opposite sides: the `agent_spawn`
    /// event is what the main thread *did*, the run is what it *cost*. They are
    /// joined by the spawn call's id — kept in the event's `target` — so the
    /// spawn's own `out_tokens` becomes the run's output and its `sub_tokens`
    /// the output of everything that run spawned in turn. A spawn whose run is
    /// missing keeps its zeros rather than borrowing another's figures.
    fn write_subagent_runs(
        tx: &rusqlite::Transaction<'_>,
        session: &SessionMeta,
        runs: &[SubagentRun],
    ) -> Result<()> {
        tx.execute(
            "DELETE FROM subagent_runs WHERE source_path = ?1",
            (&session.source_path,),
        )?;
        for run in runs {
            tx.execute(
                "INSERT INTO subagent_runs
                   (session_id, source_path, run_path, agent, agent_id, tool_use_id,
                    root_tool_use_id, parent_agent_id, spawn_depth, model, prompt_id,
                    out_tokens, started_at, started_epoch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                (
                    &session.id,
                    &session.source_path,
                    &run.run_path,
                    &run.agent,
                    &run.agent_id,
                    &run.tool_use_id,
                    &run.root_tool_use_id,
                    &run.parent_agent_id,
                    run.spawn_depth,
                    &run.model,
                    &run.prompt_id,
                    run.out_tokens,
                    epoch_ms_to_rfc3339(run.started_epoch_ms),
                    run.started_epoch_ms / 1000,
                ),
            )?;
        }
        tx.execute(
            // Every subquery is scoped to this transcript. A call id is unique
            // within a session but nothing guarantees it across sessions, and an
            // unscoped scalar subquery would silently pick an arbitrary matching
            // row while the sums added another session's runs on top.
            "UPDATE events
                SET out_tokens = COALESCE((SELECT r.out_tokens FROM subagent_runs r
                                            WHERE r.source_path = events.source_path
                                              AND r.tool_use_id = events.target), 0),
                    sub_tokens = COALESCE((SELECT SUM(r.out_tokens) FROM subagent_runs r
                                            WHERE r.source_path = events.source_path
                                              AND r.root_tool_use_id = events.target
                                              AND r.tool_use_id IS NOT events.target), 0),
                    sub_agent_count = (SELECT COUNT(*) FROM subagent_runs r
                                        WHERE r.source_path = events.source_path
                                          AND r.root_tool_use_id = events.target
                                          AND r.tool_use_id IS NOT events.target)
              WHERE kind = 'agent_spawn' AND source_path = ?1 AND target IS NOT NULL",
            (&session.source_path,),
        )?;
        Ok(())
    }

    /// Tool-failure events for a session: `(epoch_ms, category, excerpt, tool,
    /// target)` — inside the batch transaction, after [`Self::write_session`]'s
    /// replace. For a `tool_error` row the otherwise-unused detail columns carry
    /// the friction signal, per `EVENT_ENCODING`: category, excerpt, originating
    /// tool, call subject. `surface_kind` stays NULL so these never enter the
    /// surface-catalog join. The excerpt, tool, and target let a report show
    /// concrete instances, which tool produced them, and which file or command
    /// they hit — without re-reading the transcript. The columns come from the
    /// encoding table, so the writer, the view, and the readers cannot disagree.
    fn write_tool_errors(
        tx: &rusqlite::Transaction<'_>,
        session: &SessionMeta,
        errors: &[(i64, &str, &str, &str, &str)],
    ) -> Result<()> {
        let category = encoding_column("tool_error", EventRole::Category);
        let excerpt = encoding_column("tool_error", EventRole::Excerpt);
        let tool = encoding_column("tool_error", EventRole::OriginatingTool);
        let target = encoding_column("tool_error", EventRole::FailureTarget);
        let insert = format!(
            "INSERT INTO events
               (session_id, source_path, kind, {category}, {excerpt}, {tool}, {target},
                started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                ctx_start, ctx_peak)
             VALUES (?1, ?2, 'tool_error', ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, 0, 0, 0)"
        );
        for (epoch_ms, category, excerpt, tool, target) in errors {
            tx.execute(
                &insert,
                (
                    &session.id,
                    &session.source_path,
                    *category,
                    *excerpt,
                    *tool,
                    *target,
                    epoch_ms_to_rfc3339(*epoch_ms),
                    epoch_ms / 1000,
                ),
            )?;
        }
        Ok(())
    }

    /// Work events `(epoch_ms, kind, id, path)` — Bash leading words and edited
    /// file basenames — kept out of the catalog join (`surface_kind` NULL), for
    /// a session whose rows [`Self::write_session`] just replaced. A file edit's
    /// full path goes to `target` while `surface_id` keeps the basename that
    /// hotspot rankings group on; which meaning a row's value carries depends on
    /// its `kind` and is read back per `EVENT_ENCODING` (one statement spans
    /// several kinds, so the writer fills the physical columns as they are).
    fn write_work_events(
        tx: &rusqlite::Transaction<'_>,
        session: &SessionMeta,
        events: &[(i64, &str, &str, Option<&str>)],
    ) -> Result<()> {
        for (epoch_ms, kind, id, path) in events {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id, target,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, 0, 0, 0)",
                (
                    &session.id,
                    &session.source_path,
                    kind,
                    id,
                    path,
                    epoch_ms_to_rfc3339(*epoch_ms),
                    epoch_ms / 1000,
                ),
            )?;
        }
        Ok(())
    }

    /// Every file edit as a `FileEdit`, time-ordered — the input to thrash
    /// detection, which needs the session and full path to tell one agent's
    /// retries apart from parallel work on a same-named file (`core::thrash`).
    ///
    /// Rows without a recorded path are **excluded**, not fallen back to the
    /// basename in `surface_id`: grouping same-named files together is the bug
    /// this query exists to avoid, and a silent fallback would reproduce it for
    /// every store written before paths were recorded — invisibly, since the
    /// output would look normal. An empty result surfaces as "no thrash
    /// episodes", which tells the user to re-analyze.
    pub fn file_edits(&self, range: TimeRange) -> Result<Vec<FileEdit>> {
        // Which column holds the full path is EVENT_ENCODING's definition for
        // `file_edit`; this reader takes it from there instead of repeating it.
        let path = encoding_column("file_edit", EventRole::FilePath);
        let (filter, binds) = range_clause(range, "e.started_epoch");
        let sql = format!(
            "SELECT s.project, e.session_id, e.{path}, e.started_epoch
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'file_edit' AND e.{path} IS NOT NULL{filter}
             ORDER BY e.started_epoch"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok(FileEdit {
                    project: row.get(0)?,
                    session_id: row.get(1)?,
                    path: row.get(2)?,
                    epoch: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts of a work-event kind by id (e.g. Bash leading word, edited file),
    /// most frequent first.
    pub fn work_counts(&self, kind: &str, range: TimeRange) -> Result<Vec<(String, i64)>> {
        // Which meaning a work row's id carries depends on its kind, and
        // EVENT_ENCODING is where that is defined; a kind with no entry is
        // rejected rather than read from an arbitrary column.
        let id = match kind {
            "file_edit" => encoding_column("file_edit", EventRole::FileBasename),
            "bash_cmd" => encoding_column("bash_cmd", EventRole::CommandWord),
            other => anyhow::bail!("no encoding defined for work-event kind {other:?}"),
        };
        let (filter, binds) = range_clause(range, "started_epoch");
        let sql = format!(
            "SELECT {id}, COUNT(*) FROM events WHERE kind = ?1{filter}
             GROUP BY {id} ORDER BY COUNT(*) DESC"
        );
        let kind_cell = rusqlite::types::Value::Text(kind.to_string());
        let mut params: Vec<rusqlite::types::Value> = vec![kind_cell];
        params.extend(binds.iter().copied().map(rusqlite::types::Value::from));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts of tool failures by category, most frequent first.
    pub fn error_counts(&self, range: TimeRange) -> Result<Vec<(String, i64)>> {
        let category = encoding_column("tool_error", EventRole::Category);
        let (filter, binds) = range_clause(range, "started_epoch");
        let sql = format!(
            "SELECT {category}, COUNT(*) FROM events WHERE kind = 'tool_error'{filter}
             GROUP BY {category} ORDER BY COUNT(*) DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Tool-failure counts split by project and category, densest pair first.
    /// Joins each error event back to its session's project so a friction
    /// category can be attributed to the project whose config should carry the
    /// fix — backing a `--project` filter and the dominant-project line in the
    /// summary.
    pub fn error_counts_by_project(&self, range: TimeRange) -> Result<Vec<(String, String, i64)>> {
        let category = encoding_column("tool_error", EventRole::Category);
        let (filter, binds) = range_clause(range, "e.started_epoch");
        let sql = format!(
            "SELECT s.project, e.{category}, COUNT(*)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'tool_error'{filter}
             GROUP BY s.project, e.{category}
             ORDER BY COUNT(*) DESC, s.project, e.{category}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Up to `per_group` example error excerpts for each `(project, category)`,
    /// as `(project, category, excerpt)`. Gives a report the concrete instances
    /// behind a friction count — the actual failing paths/files — so the reader
    /// (or a seeded agent) need not re-mine the transcripts. Earliest examples
    /// first within a group; empty excerpts are skipped.
    pub fn error_examples(
        &self,
        per_group: u32,
        range: TimeRange,
    ) -> Result<Vec<(String, String, String)>> {
        let category = encoding_column("tool_error", EventRole::Category);
        let excerpt = encoding_column("tool_error", EventRole::Excerpt);
        let (filter, binds) = range_clause(range, "e.started_epoch");
        let sql = format!(
            "SELECT project, category, excerpt FROM (
                 SELECT s.project AS project, e.{category} AS category, e.{excerpt} AS excerpt,
                        ROW_NUMBER() OVER (
                            PARTITION BY s.project, e.{category} ORDER BY e.id
                        ) AS rn
                 FROM events e JOIN sessions s ON e.session_id = s.id
                 WHERE e.kind = 'tool_error' AND e.{excerpt} IS NOT NULL AND e.{excerpt} <> ''{filter}
             )
             WHERE rn <= ?
             ORDER BY project, category, rn"
        );
        // Anonymous placeholders throughout: the range bounds bind first (the
        // filter sits before the LIMIT), the per-group cap last.
        let mut params: Vec<rusqlite::types::Value> = binds
            .iter()
            .copied()
            .map(rusqlite::types::Value::from)
            .collect();
        params.push(rusqlite::types::Value::Integer(i64::from(per_group)));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Tool-failure counts split by project, category, and originating tool, as
    /// `(project, category, tool, count)`, densest first. Answers "which tool
    /// produced these failures" — e.g. path-not-found split across Read / Bash /
    /// Edit / a Playwright locator — so a report separates file friction from a
    /// browser miss that merely reads as "not found", and the seeded agent need
    /// not re-derive the attribution from the transcripts.
    pub fn error_tool_breakdown(
        &self,
        range: TimeRange,
    ) -> Result<Vec<(String, String, String, i64)>> {
        let category = encoding_column("tool_error", EventRole::Category);
        let tool = encoding_column("tool_error", EventRole::OriginatingTool);
        let (filter, binds) = range_clause(range, "e.started_epoch");
        let sql = format!(
            "SELECT s.project, e.{category}, COALESCE(e.{tool}, 'unknown'), COUNT(*)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'tool_error'{filter}
             GROUP BY s.project, e.{category}, e.{tool}
             ORDER BY COUNT(*) DESC, s.project, e.{category}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts of prompts by behavioral class (`source` column on prompt events),
    /// most frequent first.
    pub fn prompt_behavior_counts(&self, range: TimeRange) -> Result<Vec<(String, i64)>> {
        let behavior = encoding_column("prompt", EventRole::PromptBehavior);
        let (filter, binds) = range_clause(range, "started_epoch");
        let sql = format!(
            "SELECT {behavior}, COUNT(*) FROM events WHERE kind = 'prompt' AND {behavior} IS NOT NULL{filter}
             GROUP BY {behavior} ORDER BY COUNT(*) DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Per-skill usage rollup, most-invoked first.
    pub fn skill_usage(&self, range: TimeRange) -> Result<Vec<SkillUsage>> {
        let (filter, binds) = range_clause(range, "started_epoch");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT surface_id,
                    COUNT(*),
                    SUM(out_tokens),
                    SUM(ctx_growth),
                    SUM(duration_sec)
             FROM events
             WHERE surface_kind = 'skill'{filter}
             GROUP BY surface_id
             ORDER BY COUNT(*) DESC, surface_id"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok(SkillUsage {
                    skill: row.get(0)?,
                    invocations: row.get(1)?,
                    out_tokens: row.get(2)?,
                    ctx_growth: row.get(3)?,
                    duration_sec: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Total startup tokens of **global** config — what every session pays
    /// unconditionally from `~/.claude` regardless of project: always-on files
    /// in full, plus the descriptions skills and agents contribute to the
    /// startup listing. Bodies that wait to be invoked weigh nothing here, and a
    /// surface whose startup weight is unknown (an MCP schema) is left out of
    /// the sum rather than counted as zero. Project config is always-on only for
    /// its own sessions (`always_on_config_tokens_for`).
    pub fn always_on_config_tokens(&self) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(startup_tokens), 0) FROM surfaces
             WHERE scope = 'global'",
            [],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    /// Empirical always-on context floor per project as `(project, floor,
    /// sessions)`: the leanest context any of that project's sessions *started*
    /// with, and how many starts back the figure.
    ///
    /// Only `session_start` events qualify. A skill span's `ctx_start` is the
    /// prompt size wherever that skill ran, which in a long or resumed session
    /// sits far above the session's own start — reading floors from spans
    /// reported startup costs that were really mid-session sizes. The session
    /// count is part of the answer because the floor is a minimum over
    /// observations: one resumed session alone still overstates it, and the
    /// count is what makes that visible.
    pub fn baseline_floor_per_project(&self, range: TimeRange) -> Result<Vec<(String, i64, i64)>> {
        let (filter, binds) = range_clause_utc_text(range, "s.started_at");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.project, MIN(e.ctx_start), COUNT(*)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'session_start' AND e.ctx_start > 0{filter}
             GROUP BY s.project
             ORDER BY MIN(e.ctx_start) DESC"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The global empirical always-on floor — the leanest session start observed
    /// anywhere. `None` when no session start was observed at all, so callers
    /// report the metric as unavailable instead of inventing a zero.
    pub fn baseline_floor(&self, range: TimeRange) -> Result<Option<i64>> {
        let (filter, binds) = range_clause_utc_text(range, "s.started_at");
        let floor = self.conn.query_row(
            &format!(
                "SELECT MIN(e.ctx_start) FROM events e
                 JOIN sessions s ON e.session_id = s.id
                 WHERE e.kind = 'session_start' AND e.ctx_start > 0{filter}"
            ),
            params_from_iter(binds),
            |row| row.get(0),
        )?;
        Ok(floor)
    }

    /// How much data the store holds: `(sessions, distinct projects)` — the
    /// summary's "what was analyzed" context line.
    pub fn session_stats(&self, range: TimeRange) -> Result<(i64, i64)> {
        let (filter, binds) = range_clause_utc_text(range, "started_at");
        let row = self.conn.query_row(
            &format!("SELECT COUNT(*), COUNT(DISTINCT project) FROM sessions WHERE 1=1{filter}"),
            params_from_iter(binds),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(row)
    }

    /// Total subagent output tokens and subagent count across all sessions.
    pub fn subagent_totals(&self, range: TimeRange) -> Result<(i64, i64)> {
        let (filter, binds) = range_clause_utc_text(range, "started_at");
        let row = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(sub_tokens), 0), COALESCE(SUM(sub_agent_count), 0)
                 FROM sessions WHERE 1=1{filter}"
            ),
            params_from_iter(binds),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(row)
    }

    /// Subagent output tokens and run count per agent type, costliest first —
    /// the breakdown behind the session-level subagent total.
    ///
    /// Every run counts once, at its own cost, whatever its depth: a nested run
    /// is the agent that produced those tokens, and rolling it into its parent's
    /// type would credit the cost to an agent that only delegated. Runs whose
    /// type is unknown (no sidecar beside the transcript) group under `None`
    /// rather than being dropped, so the rows sum to the total of every run
    /// that was extracted. Sessions whose transcripts were pruned before their
    /// runs could be extracted keep their totals but have no rows here; a
    /// report reconciles the two with `core::subagent::SplitCoverage` rather
    /// than presenting these rows as the whole breakdown.
    pub fn subagent_by_agent(&self, range: TimeRange) -> Result<Vec<AgentCost>> {
        let (filter, binds) = range_clause(range, "started_epoch");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT agent, COUNT(*), COALESCE(SUM(out_tokens), 0)
             FROM subagent_runs
             WHERE 1=1{filter}
             GROUP BY agent
             ORDER BY COALESCE(SUM(out_tokens), 0) DESC, COUNT(*) DESC"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok(AgentCost {
                    agent: row.get(0)?,
                    runs: row.get(1)?,
                    out_tokens: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Invocation counts per surface `(kind, id)` across all event kinds — the
    /// usage side of the catalog join for every surface, not just skills.
    pub fn usage_counts(&self, range: TimeRange) -> Result<Vec<(String, String, i64)>> {
        let (filter, binds) = range_clause(range, "started_epoch");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT surface_kind, surface_id, COUNT(*)
             FROM events
             WHERE surface_kind IS NOT NULL{filter}
             GROUP BY surface_kind, surface_id"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Per-event costs for skill invocations, for time bucketing in the report.
    pub fn skill_event_costs(&self, range: TimeRange) -> Result<Vec<EventCost>> {
        let (filter, binds) = range_clause(range, "started_epoch");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT started_epoch, out_tokens, ctx_growth, duration_sec
             FROM events
             WHERE surface_kind = 'skill'{filter}"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok(EventCost {
                    started_epoch: row.get(0)?,
                    out_tokens: row.get(1)?,
                    ctx_growth: row.get(2)?,
                    duration_sec: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// An analyze-run metadata value, if recorded.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query_map([key], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    /// Record an analyze-run metadata value, superseding any previous one.
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            (key, value),
        )?;
        Ok(())
    }

    /// Whether the store's rows were produced by the current analysis semantics.
    /// The `(mtime, size)` skip below never expires, so an improved extractor or
    /// classifier would otherwise leave a transcript that never changes stamped
    /// with its old results forever; a mismatch here tells `analyze` to re-ingest
    /// everything once (`docs/specs/storage.md`). "Everything" means every
    /// transcript still on disk — rows whose source file is gone can never be
    /// re-derived, so they stay as last extracted.
    ///
    /// Which `key`/`version` pair a harness uses is the harness's own business
    /// (`adapter::transcript` for Claude Code, `adapter::opencode`,
    /// `adapter::kilocode`); the store only compares what it was handed, so it
    /// stays harness-agnostic.
    pub fn is_analyzer_current(&self, key: &str, version: &str) -> Result<bool> {
        Ok(self.meta(key)?.as_deref() == Some(version))
    }

    /// Stamp the store with the analysis semantics its rows were produced by.
    /// Call once a full analyze run has completed.
    pub fn record_analyzer_version(&mut self, key: &str, version: &str) -> Result<()> {
        self.set_meta(key, version)
    }

    /// Whether a source file's `(mtime, size)` matches its recorded ingest
    /// fingerprint — if so, re-analyzing may skip it (`docs/specs/storage.md`).
    pub fn is_ingested(&self, path: &str, mtime: i64, size: i64) -> Result<bool> {
        let matched = self.conn.query_row(
            "SELECT COUNT(*) FROM ingested_files WHERE path = ?1 AND mtime = ?2 AND size = ?3",
            (path, mtime, size),
            |row| row.get::<_, i64>(0),
        )?;
        Ok(matched > 0)
    }

    /// Record a source file's ingest fingerprint. Call **after** the read
    /// completes, with the pre-read stat — a file that grew mid-read then shows
    /// a changed fingerprint next run and is re-ingested (`docs/specs/storage.md`).
    pub fn record_ingested_file(&mut self, path: &str, mtime: i64, size: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ingested_files (path, mtime, size) VALUES (?1, ?2, ?3)",
            (path, mtime, size),
        )?;
        Ok(())
    }

    /// Rebuild the surface catalog wholesale — it is a snapshot of current
    /// config, not an accumulation (see `docs/specs/storage.md`).
    pub fn replace_surfaces(&mut self, surfaces: &[Surface]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM surfaces", ())?;
        for surface in surfaces {
            tx.execute(
                "INSERT OR REPLACE INTO surfaces
                   (kind, id, scope, project, config_path, static_tokens,
                    startup_tokens, load_mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    &surface.kind,
                    &surface.id,
                    surface.scope.label(),
                    surface.scope.project(),
                    &surface.config_path,
                    surface.static_tokens,
                    surface.startup_tokens,
                    surface.load_mode.label(),
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The whole catalog — every `(kind, id, scope, project)` row — with each
    /// row's **effective** usage. Shadowing is per project: an event from
    /// project P joins P's project row when one exists for that `(kind, id)`,
    /// else the global row. One event therefore counts on exactly one row
    /// (`docs/specs/surfaces.md`).
    pub fn effective_catalog(&self, range: TimeRange) -> Result<Vec<CatalogEntry>> {
        let (filter, binds) = range_clause(range, "e.started_epoch");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT f.kind, f.id, f.scope, f.project, f.static_tokens,
                    f.startup_tokens, f.load_mode, COUNT(u.event_id)
             FROM surfaces f
             LEFT JOIN (SELECT e.id AS event_id, e.surface_kind, e.surface_id,
                               s.project AS session_project
                        FROM events e JOIN sessions s ON e.session_id = s.id
                        WHERE e.surface_kind IS NOT NULL{filter}) u
               ON u.surface_kind = f.kind AND u.surface_id = f.id
              AND ((f.scope = 'project' AND u.session_project = f.project)
                OR (f.scope = 'global' AND u.session_project NOT IN (
                      SELECT p.project FROM surfaces p
                      WHERE p.kind = f.kind AND p.id = f.id AND p.scope = 'project')))
             GROUP BY f.kind, f.id, f.scope, f.project
             ORDER BY f.kind, f.id, f.scope, f.project"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(binds), |row| {
                Ok(CatalogEntry {
                    kind: row.get(0)?,
                    id: row.get(1)?,
                    scope: row.get(2)?,
                    project: row.get(3)?,
                    static_tokens: row.get(4)?,
                    startup_tokens: row.get(5)?,
                    load_mode: row.get(6)?,
                    uses: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct known `(root, project)` pairs — the directories project-config
    /// scanning walks, each with the normalized project slug its surfaces get
    /// scoped to. Sessions whose transcript carried no cwd are skipped.
    pub fn session_roots(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT root, project FROM sessions WHERE root <> '' ORDER BY root",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Total startup config tokens for a session in `project`: the global
    /// figure plus that project's own startup cost.
    ///
    /// A surface installed at both scopes under the same `(kind, id)` is counted
    /// twice here. Resolving that would mean asserting which copy Claude Code
    /// actually loads, and the shadowing model this store implements is defined
    /// for the **usage join** (`effective_catalog`), not for load order — a
    /// project rule and its global namesake genuinely both reach the context, so
    /// excluding one would understate the figure instead.
    pub fn always_on_config_tokens_for(&self, project: &str) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(startup_tokens), 0) FROM surfaces
             WHERE scope = 'global' OR (scope = 'project' AND project = ?1)",
            [project],
            |row| row.get(0),
        )?;
        Ok(total)
    }
}

/// Bring an existing store onto the current schema **in place**. The store
/// outlives the transcripts it was built from, so an upgrade may not resolve
/// schema drift by asking for the file to be deleted — that would discard
/// history no re-analysis can reconstruct (`docs/specs/storage.md`).
///
/// Drift is reconciled per table by what the table costs to lose, and the
/// declared schema is the single source of truth for the target shape: it is
/// applied to a scratch database and read back, so adding a column to `SCHEMA`
/// is all a future change needs.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        // Reconciliation only ever adds; it cannot undo a newer saniwalens's
        // changes, and writing this build's shape over them would corrupt a
        // store that binary still reads correctly.
        anyhow::bail!(
            "the store was written by a newer saniwalens (schema v{version}; this build knows \
             v{SCHEMA_VERSION}) — upgrade saniwalens to read it"
        );
    }

    let desired = Connection::open_in_memory()?;
    desired.execute_batch(SCHEMA)?;
    desired.execute_batch(INDEX_SCHEMA)?;
    desired.execute_batch(&view_schema())?;

    // One transaction for the whole migration, as every other multi-statement
    // write in this file does: reconciliation may bail partway through, and a
    // store left half-migrated is exactly the outcome in-place upgrading exists
    // to avoid. SQLite rolls DDL back like any other statement.
    let tx = conn.transaction()?;

    let mut rebuilt = false;
    for table in REBUILT_TABLES {
        if table_exists(&tx, table)? && table_shape(&tx, table)? != table_shape(&desired, table)? {
            tx.execute_batch(&format!("DROP TABLE {table}"))?;
            rebuilt = true;
        }
    }
    tx.execute_batch(SCHEMA)?;
    for table in MIGRATED_TABLES {
        reconcile_columns(&tx, &desired, table)?;
    }
    // Indexes and views come last: both reference columns the step above may
    // have just added.
    reconcile_derived(&tx, &desired, "index")?;
    reconcile_derived(&tx, &desired, "view")?;
    if rebuilt {
        // The catalog is empty until the next analyze refills it. Without this a
        // `--frozen` read would print an ordinary freshness line over an empty
        // catalog, reporting "nothing installed" as though it were a finding.
        tx.execute("DELETE FROM meta WHERE key = 'analyzed_at'", ())?;
    }

    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

/// Reconcile `table` against the columns `desired` declares for it. Only
/// additive drift is reconcilable in place — SQLite can append a column but not
/// retype or re-key one, and a NOT NULL column needs a default to fill the rows
/// already stored — so anything else is reported rather than half-applied.
///
/// A column already present is compared, not skipped: reconciliation would
/// otherwise pass silently over a retype or a changed default, leaving the store
/// permanently diverged from the declaration it is supposed to converge on.
fn reconcile_columns(conn: &Connection, desired: &Connection, table: &str) -> Result<()> {
    let present = table_shape(conn, table)?;
    for column in table_shape(desired, table)? {
        if let Some(existing) = present.iter().find(|c| c.name == column.name) {
            if *existing != column {
                anyhow::bail!(
                    "`{table}.{}` is declared as {column:?} but the store holds {existing:?}: \
                     SQLite cannot change a column in place, so this needs an explicit \
                     migration that states what it discards",
                    column.name
                );
            }
            continue;
        }
        let mut ddl = format!(
            "ALTER TABLE {table} ADD COLUMN {} {}",
            column.name, column.decl_type
        );
        match &column.default {
            Some(default) => {
                if column.notnull {
                    ddl.push_str(" NOT NULL");
                }
                ddl.push_str(&format!(" DEFAULT {default}"));
            }
            None if column.notnull => anyhow::bail!(
                "cannot add `{table}.{}` to an existing store: a NOT NULL column needs a \
                 DEFAULT to fill the rows already stored",
                column.name
            ),
            None => {}
        }
        conn.execute_batch(&ddl)?;
    }
    Ok(())
}

/// Recreate every index or view of `kind` whose stored definition differs from
/// the declared one. Both carry no data, so replacing one costs nothing but the
/// rebuild — which is why only a *drifted* object is touched: reindexing a large
/// `events` table on every open would not be free. Objects the declaration does
/// not name are left alone; the store is meant to be explored with ad-hoc SQL, so
/// a helper view someone saved into the file is theirs to keep.
///
/// Deriving the names from the declaration is also what removes the hand-kept
/// list this replaced: such a list drifts, and a declared object missing from it
/// would collide with its own `CREATE` on the store's second open.
fn reconcile_derived(conn: &Connection, desired: &Connection, kind: &str) -> Result<()> {
    let stored = declared_sql(conn, kind)?;
    for (name, sql) in declared_sql(desired, kind)? {
        if stored.get(&name).is_some_and(|existing| *existing == sql) {
            continue;
        }
        conn.execute_batch(&format!("DROP {kind} IF EXISTS {name}"))?;
        conn.execute_batch(&sql)?;
    }
    Ok(())
}

/// `name -> CREATE statement` for every object of `kind` that carries one (an
/// implicit index has none). SQLite stores the statement as written minus
/// `IF NOT EXISTS`, so definitions built from the same declaration compare equal.
fn declared_sql(conn: &Connection, kind: &str) -> Result<BTreeMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = ?1 AND sql IS NOT NULL")?;
    let rows = stmt
        .query_map([kind], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, String>>>()?;
    Ok(rows)
}

/// One column as SQLite reports it (`PRAGMA table_info`).
#[derive(Debug, PartialEq)]
struct ColumnDef {
    name: String,
    decl_type: String,
    notnull: bool,
    default: Option<String>,
    pk: i64,
}

/// A table's columns in declaration order; empty when the table does not exist.
fn table_shape(conn: &Connection, table: &str) -> Result<Vec<ColumnDef>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| {
            Ok(ColumnDef {
                name: row.get(1)?,
                decl_type: row.get(2)?,
                notnull: row.get::<_, i64>(3)? != 0,
                default: row.get(4)?,
                pk: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

fn source_label(source: Source) -> &'static str {
    match source {
        Source::Slash => "slash",
        Source::Tool => "tool",
    }
}

fn epoch_ms_to_rfc3339(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// A SQLite cell as a typed JSON value, for the `--format json` surface.
fn value_to_json(v: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::json!(i),
        ValueRef::Real(f) => serde_json::json!(f),
        ValueRef::Text(t) => serde_json::json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(_) => serde_json::json!("<blob>"),
    }
}

/// Stringify a SQLite cell for the generic `query` surface.
fn value_to_string(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(_) => "<blob>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::period::TimeRange;
    use crate::core::surface::{LoadMode, Scope};

    /// The store is agnostic to which analysis-semantics pair a harness
    /// stamps, so the tests use their own — nothing here depends on a real
    /// harness's key or version.
    const TEST_ANALYZER_KEY: &str = "test_analyzer_version";
    const TEST_ANALYZER_VERSION: &str = "1";

    /// Permissions are the reason this test and the next one work on a real
    /// temp directory instead of `Store::in_memory`: the default store path
    /// lives under a user state directory that may not exist yet, and both it
    /// and the store must come out owner-only.
    #[test]
    fn open_creates_a_missing_directory_and_keeps_both_owner_only() {
        let dir = std::env::temp_dir().join(format!("saniwalens-open-test-{}", std::process::id()));
        let path = dir.join("saniwalens").join("saniwalens.db");
        let _ = std::fs::remove_dir_all(&dir);

        drop(Store::open(&path, OpenMode::ReadWrite).unwrap());

        assert!(path.exists(), "the store was not created at {path:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The store an older saniwalens left under the umask is the case that actually
    /// exists in the wild, so opening it has to narrow it — otherwise the
    /// protection never reaches anyone who already ran the tool.
    #[cfg(unix)]
    #[test]
    fn open_narrows_a_store_an_earlier_run_left_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("saniwalens-mode-test-{}", std::process::id()));
        let path = dir.join("saniwalens.db");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        drop(Store::open(&path, OpenMode::ReadWrite).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        drop(Store::open(&path, OpenMode::ReadWrite).unwrap());

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn span(skill: &str, out_tokens: u64, ctx_growth: u64, duration_sec: f64) -> Span {
        Span {
            skill: skill.to_string(),
            source: Source::Slash,
            started_epoch_ms: 1_700_000_000_000,
            duration_sec,
            out_tokens,
            ctx_growth,
            ctx_start: 0,
            ctx_peak: ctx_growth,
            model: Some("claude-opus-4-7".to_string()),
            is_trailing: false,
            spawns: Vec::new(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    fn session(id: &str) -> SessionMeta {
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

    fn agent_spawn(surface_id: &str, tool_use_id: &str) -> UsageEvent {
        UsageEvent {
            surface_kind: "agent".to_string(),
            surface_id: surface_id.to_string(),
            started_epoch_ms: 1_700_000_000_000,
            tool_use_id: Some(tool_use_id.to_string()),
        }
    }

    fn subagent_run(agent: &str, agent_id: &str, root: &str, out_tokens: u64) -> SubagentRun {
        SubagentRun {
            agent: Some(agent.to_string()),
            agent_id: agent_id.to_string(),
            run_path: format!("/tmp/example/subagents/agent-{agent_id}.jsonl"),
            tool_use_id: Some(format!("toolu_{agent_id}")),
            parent_agent_id: None,
            spawn_depth: 1,
            model: Some("sonnet".to_string()),
            prompt_id: Some("p1".to_string()),
            out_tokens,
            started_epoch_ms: 1_700_000_000_000,
            root_tool_use_id: Some(root.to_string()),
        }
    }

    #[test]
    fn subagent_cost_rolls_up_per_agent_type() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store
            .ingest(IngestBatch::new(&s1, &[], &[]).subagent_runs(&[
                subagent_run("Explore", "a1", "toolu_a1", 400),
                subagent_run("Explore", "a2", "toolu_a2", 100),
                subagent_run("code-writer", "a3", "toolu_a3", 900),
            ]))
            .unwrap();

        assert_eq!(
            store.subagent_by_agent(TimeRange::ALL).unwrap(),
            vec![
                AgentCost {
                    agent: Some("code-writer".to_string()),
                    runs: 1,
                    out_tokens: 900,
                },
                AgentCost {
                    agent: Some("Explore".to_string()),
                    runs: 2,
                    out_tokens: 500,
                },
            ]
        );
    }

    #[test]
    fn a_spawn_event_takes_the_cost_of_the_run_it_started() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        let mut nested = subagent_run("code-writer", "a2", "toolu_a1", 700);
        nested.parent_agent_id = Some("a1".to_string());
        nested.spawn_depth = 2;
        store
            .ingest(
                IngestBatch::new(&s1, &[], &[agent_spawn("Explore", "toolu_a1")])
                    .subagent_runs(&[subagent_run("Explore", "a1", "toolu_a1", 400), nested]),
            )
            .unwrap();

        let (out_tokens, sub_tokens, sub_agents) = store
            .query(
                "SELECT out_tokens, sub_tokens, sub_agent_count FROM events \
                 WHERE kind = 'agent_spawn'",
            )
            .map(|(_, rows)| (rows[0][0].clone(), rows[0][1].clone(), rows[0][2].clone()))
            .unwrap();

        // The spawn's own run wrote 400; the run it spawned in turn wrote 700.
        assert_eq!((out_tokens.as_str(), sub_tokens.as_str()), ("400", "700"));
        assert_eq!(sub_agents, "1");
    }

    #[test]
    fn a_spawn_whose_run_is_missing_keeps_its_zeros() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store
            .ingest(
                IngestBatch::new(&s1, &[], &[agent_spawn("Explore", "toolu_gone")])
                    .subagent_runs(&[subagent_run("Explore", "a1", "toolu_a1", 400)]),
            )
            .unwrap();

        let (_, rows) = store
            .query("SELECT out_tokens, sub_tokens FROM events WHERE target = 'toolu_gone'")
            .unwrap();
        assert_eq!(rows, vec![vec!["0".to_string(), "0".to_string()]]);
    }

    #[test]
    fn a_spawn_takes_only_its_own_sessions_run() {
        // Call ids are unique within a session, not across the store — a resumed
        // or copied transcript can carry the same id into a second session.
        let mut store = Store::in_memory().unwrap();
        let (s1, s2) = (session("s1"), session("s2"));
        store
            .ingest(
                IngestBatch::new(&s1, &[], &[agent_spawn("Explore", "toolu_a1")])
                    .subagent_runs(&[subagent_run("Explore", "a1", "toolu_a1", 400)]),
            )
            .unwrap();
        store
            .ingest(
                IngestBatch::new(&s2, &[], &[agent_spawn("Explore", "toolu_a1")])
                    .subagent_runs(&[subagent_run("Explore", "a1", "toolu_a1", 900)]),
            )
            .unwrap();

        let (_, rows) = store
            .query(
                "SELECT session_id, out_tokens FROM events \
                 WHERE kind = 'agent_spawn' ORDER BY session_id",
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["s1".to_string(), "400".to_string()],
                vec!["s2".to_string(), "900".to_string()],
            ]
        );
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_subagent_runs() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest(IngestBatch::new(&s1, &[], &[])).unwrap();
        let runs = [subagent_run("Explore", "a1", "toolu_a1", 400)];
        store
            .ingest(IngestBatch::new(&s1, &[], &[]).subagent_runs(&runs))
            .unwrap();
        store
            .ingest(IngestBatch::new(&s1, &[], &[]).subagent_runs(&runs))
            .unwrap();

        assert_eq!(
            store.subagent_by_agent(TimeRange::ALL).unwrap(),
            vec![AgentCost {
                agent: Some("Explore".to_string()),
                runs: 1,
                out_tokens: 400,
            }]
        );
    }

    #[test]
    fn rolls_up_usage_per_skill_across_sessions() {
        let mut store = Store::in_memory().unwrap();
        store
            .ingest(IngestBatch::new(
                &session("s1"),
                &[
                    span("git-commit", 100, 50, 2.0),
                    span("git-commit", 200, 30, 1.0),
                ],
                &[],
            ))
            .unwrap();
        store
            .ingest(IngestBatch::new(
                &session("s2"),
                &[span("pr-create", 10, 5, 0.5)],
                &[],
            ))
            .unwrap();

        let usage = store.skill_usage(TimeRange::ALL).unwrap();

        assert_eq!(
            usage,
            vec![
                SkillUsage {
                    skill: "git-commit".to_string(),
                    invocations: 2,
                    out_tokens: 300,
                    ctx_growth: 80,
                    duration_sec: 3.0,
                },
                SkillUsage {
                    skill: "pr-create".to_string(),
                    invocations: 1,
                    out_tokens: 10,
                    ctx_growth: 5,
                    duration_sec: 0.5,
                },
            ]
        );
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_events() {
        let mut store = Store::in_memory().unwrap();
        store
            .ingest(IngestBatch::new(
                &session("s1"),
                &[span("git-commit", 100, 50, 2.0)],
                &[],
            ))
            .unwrap();
        // Same source_path, different content — must supersede, not accumulate.
        store
            .ingest(IngestBatch::new(
                &session("s1"),
                &[span("git-commit", 999, 999, 9.0)],
                &[],
            ))
            .unwrap();

        let usage = store.skill_usage(TimeRange::ALL).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].invocations, 1);
        assert_eq!(usage[0].out_tokens, 999);
    }

    /// A skill surface whose body weighs `static_tokens` and whose description
    /// (the only part loaded at startup) weighs `startup_tokens`.
    fn skill_surface(id: &str, scope: Scope, static_tokens: u64, startup_tokens: u64) -> Surface {
        Surface {
            kind: "skill".to_string(),
            id: id.to_string(),
            scope,
            config_path: format!("/cfg/{id}"),
            static_tokens: Some(static_tokens),
            startup_tokens: Some(startup_tokens),
            load_mode: LoadMode::StartupDescription,
        }
    }

    fn surface(id: &str, scope: Scope, static_tokens: u64) -> Surface {
        skill_surface(id, scope, static_tokens, 0)
    }

    #[test]
    fn a_project_surface_shadows_global_only_for_its_own_sessions() {
        // skill/git-commit is installed globally AND in project alpha; alpha and
        // beta each invoke it once. Alpha's use lands on alpha's project row,
        // beta's on the global row — two uses total, never double-counted, and
        // beta's usage never inflates alpha's copy.
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store
            .ingest(IngestBatch::new(
                &alpha,
                &[span("git-commit", 1, 1, 1.0)],
                &[],
            ))
            .unwrap();
        store
            .ingest(IngestBatch::new(
                &beta,
                &[span("git-commit", 1, 1, 1.0)],
                &[],
            ))
            .unwrap();
        store
            .replace_surfaces(&[
                surface("git-commit", Scope::Global, 100),
                surface("git-commit", Scope::Project("alpha".to_string()), 250),
            ])
            .unwrap();

        let catalog = store.effective_catalog(TimeRange::ALL).unwrap();

        assert_eq!(catalog.len(), 2);
        let global = catalog.iter().find(|e| e.scope == "global").unwrap();
        let project = catalog.iter().find(|e| e.scope == "project").unwrap();
        assert_eq!(project.project, "alpha");
        assert_eq!(project.static_tokens, Some(250));
        assert_eq!(project.uses, 1); // alpha's invocation only
        assert_eq!(global.uses, 1); // beta's invocation only
    }

    #[test]
    fn an_unshadowed_global_surface_counts_usage_from_every_project() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store
            .ingest(IngestBatch::new(
                &alpha,
                &[span("pr-create", 1, 1, 1.0)],
                &[],
            ))
            .unwrap();
        store
            .ingest(IngestBatch::new(
                &beta,
                &[span("pr-create", 1, 1, 1.0)],
                &[],
            ))
            .unwrap();
        store
            .replace_surfaces(&[surface("pr-create", Scope::Global, 40)])
            .unwrap();

        let catalog = store.effective_catalog(TimeRange::ALL).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].uses, 2);
    }

    #[test]
    fn session_roots_are_distinct_pairs_and_skip_unknown() {
        let mut store = Store::in_memory().unwrap();
        let mut a = session("a1");
        a.root = "/tmp/example/app".to_string();
        a.project = "alpha".to_string();
        let mut b = session("b1");
        b.root = "/tmp/example/app".to_string(); // duplicate root
        b.project = "alpha".to_string();
        let c = session("c1"); // root unknown (empty)
        store.ingest(IngestBatch::new(&a, &[], &[])).unwrap();
        store.ingest(IngestBatch::new(&b, &[], &[])).unwrap();
        store.ingest(IngestBatch::new(&c, &[], &[])).unwrap();

        assert_eq!(
            store.session_roots().unwrap(),
            vec![("/tmp/example/app".to_string(), "alpha".to_string())]
        );
    }

    /// A store as an older saniwalens left it: `sessions` without the columns later
    /// releases added, and `surfaces` still keyed without `project`.
    fn legacy_store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id          TEXT PRIMARY KEY,
                 project     TEXT NOT NULL,
                 slug        TEXT NOT NULL,
                 source_path TEXT NOT NULL,
                 started_at  TEXT NOT NULL
             );
             CREATE TABLE surfaces (
                 kind          TEXT NOT NULL,
                 id            TEXT NOT NULL,
                 scope         TEXT NOT NULL,
                 config_path   TEXT,
                 static_tokens INTEGER,
                 load_mode     TEXT NOT NULL,
                 PRIMARY KEY (kind, id, scope)
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_legacy_store_gains_missing_columns_without_losing_its_history() {
        // The rows are irreplaceable once Claude Code has pruned the source
        // transcripts, so upgrading must add the columns in place.
        let conn = legacy_store();
        conn.execute(
            "INSERT INTO sessions (id, project, slug, source_path, started_at)
             VALUES ('s1', 'demo', 'demo', '/tmp/example/s1.jsonl', '2026-01-01T00:00:00Z')",
            (),
        )
        .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let (_, rows) = store
            .query("SELECT id, root, sub_tokens FROM sessions")
            .unwrap();
        assert_eq!(
            rows,
            vec![vec!["s1".to_string(), String::new(), "0".to_string()]],
            "the pre-existing session must survive, with the added columns defaulted"
        );
    }

    #[test]
    fn a_legacy_catalog_table_is_rebuilt_when_its_shape_drifted() {
        // `surfaces` is a snapshot of live config, rebuilt on every analyze, so
        // a shape change is recreated rather than patched column by column.
        let conn = legacy_store();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let names: Vec<String> = table_shape(&store.conn, "surfaces")
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert!(names.contains(&"project".to_string()), "{names:?}");
    }

    #[test]
    fn an_up_to_date_catalog_survives_reopening() {
        // Reopening must not drop `surfaces` merely because the store predates
        // version stamping — a `--frozen` read would then report an empty catalog.
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[surface("git-commit", Scope::Global, 1)])
            .unwrap();
        store
            .conn
            .execute_batch("PRAGMA user_version = 0;")
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        assert_eq!(store.effective_catalog(TimeRange::ALL).unwrap().len(), 1);
    }

    #[test]
    fn a_stale_view_definition_is_replaced_on_open() {
        // `CREATE VIEW IF NOT EXISTS` leaves an older definition in place, so a
        // reader would silently query yesterday's column mapping.
        let conn = legacy_store();
        conn.execute_batch("CREATE VIEW tool_errors AS SELECT id AS session_id FROM sessions;")
            .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'tool_errors'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("kind = 'tool_error'"), "stale view: {sql}");
    }

    #[test]
    fn opening_stamps_the_current_schema_version() {
        let store = Store::from_connection(legacy_store()).unwrap();

        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_store_written_by_a_newer_saniwalens_is_refused_without_advising_deletion() {
        // Writing to it with this binary's schema would corrupt history the
        // newer saniwalens can still read; deleting it would throw that history away.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();

        let err = Store::from_connection(conn).err().expect("must be refused");

        let msg = err.to_string();
        assert!(msg.contains("upgrade"), "{msg}");
        assert!(!msg.contains("delete"), "must not advise deletion: {msg}");
    }

    #[test]
    fn a_column_that_cannot_be_added_in_place_is_reported() {
        // SQLite cannot add a NOT NULL column without a default; failing loudly
        // beats leaving the store half-migrated.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
        let desired = Connection::open_in_memory().unwrap();
        desired
            .execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, tag TEXT NOT NULL);")
            .unwrap();

        let err = reconcile_columns(&conn, &desired, "t").expect_err("must be reported");

        assert!(err.to_string().contains("tag"), "{err}");
    }

    #[test]
    fn a_column_whose_declaration_drifted_is_reported() {
        // Only additive drift is reconcilable: SQLite cannot retype or re-key a
        // column in place, so a declaration change that reconciliation would
        // silently skip has to fail while it is still a development mistake.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, n TEXT NOT NULL DEFAULT '');")
            .unwrap();
        let desired = Connection::open_in_memory().unwrap();
        desired
            .execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0);")
            .unwrap();

        let err = reconcile_columns(&conn, &desired, "t").expect_err("must be reported");

        assert!(err.to_string().contains('n'), "{err}");
    }

    #[test]
    fn a_failed_migration_leaves_the_store_untouched() {
        // The whole point of migrating in place is that the file is never left
        // worse than it was found, so a bail partway through must roll back the
        // columns already added.
        let mut conn = legacy_store();
        conn.execute_batch(
            "CREATE TABLE events (
                 id            INTEGER PRIMARY KEY,
                 session_id    TEXT NOT NULL,
                 source_path   TEXT NOT NULL,
                 kind          TEXT NOT NULL,
                 started_at    TEXT NOT NULL,
                 started_epoch INTEGER NOT NULL,
                 duration_sec  REAL NOT NULL,
                 out_tokens    TEXT NOT NULL,
                 ctx_growth    INTEGER NOT NULL,
                 ctx_start     INTEGER NOT NULL,
                 ctx_peak      INTEGER NOT NULL
             );",
        )
        .unwrap();

        migrate(&mut conn).expect_err("the drifted events.out_tokens must be reported");

        let names: Vec<String> = table_shape(&conn, "sessions")
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert!(
            !names.contains(&"root".to_string()),
            "sessions was altered before the bail and not rolled back: {names:?}"
        );
    }

    #[test]
    fn a_drifted_index_is_recreated_from_the_declaration() {
        // `CREATE INDEX IF NOT EXISTS` leaves an older definition in place, so
        // the store would keep an index the declaration no longer describes.
        let store = Store::in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "DROP INDEX events_by_surface;
                 CREATE INDEX events_by_surface ON events(surface_id);",
            )
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'events_by_surface'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("surface_kind"), "stale index: {sql}");
    }

    #[test]
    fn rebuilding_the_catalog_clears_the_analyze_timestamp() {
        // The rebuilt catalog is empty until the next analyze repopulates it, and
        // a `--frozen` read would otherwise report that emptiness as a finished
        // analysis. Dropping the timestamp routes it to the freshness warning.
        let mut store = Store::in_memory().unwrap();
        store
            .set_meta("analyzed_at", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .conn
            .execute_batch(
                "DROP TABLE surfaces;
                 CREATE TABLE surfaces (
                     kind TEXT NOT NULL, id TEXT NOT NULL, scope TEXT NOT NULL,
                     config_path TEXT, static_tokens INTEGER, load_mode TEXT NOT NULL,
                     PRIMARY KEY (kind, id, scope)
                 );",
            )
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        assert_eq!(store.meta("analyzed_at").unwrap(), None);
    }

    #[test]
    fn a_view_the_declaration_does_not_name_is_left_alone() {
        // The store is meant to be explored with ad-hoc SQL, so a helper view
        // someone saved into the file is theirs — reconciliation replaces what the
        // declaration names and touches nothing else.
        let store = Store::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        store
            .conn
            .execute_batch("CREATE VIEW stray AS SELECT id FROM sessions;")
            .unwrap();

        let store = Store::from_connection(store.conn).expect("a second open must not collide");

        let views: Vec<String> = declared_sql(&store.conn, "view")
            .unwrap()
            .into_keys()
            .collect();
        assert_eq!(views, vec!["stray".to_string(), "tool_errors".to_string()]);
    }

    #[test]
    fn an_index_is_created_after_the_columns_it_covers() {
        // Indexes are declared apart from the tables so they are applied only
        // after column reconciliation. Declaring one alongside the table would
        // run it against a legacy shape and fail the whole migration — the exact
        // destructive refusal in-place upgrading exists to remove.
        let conn = legacy_store();
        conn.execute_batch(
            "CREATE TABLE events (
                 id            INTEGER PRIMARY KEY,
                 session_id    TEXT NOT NULL,
                 source_path   TEXT NOT NULL,
                 kind          TEXT NOT NULL,
                 surface_id    TEXT,
                 started_at    TEXT NOT NULL,
                 started_epoch INTEGER NOT NULL,
                 duration_sec  REAL NOT NULL,
                 out_tokens    INTEGER NOT NULL,
                 ctx_growth    INTEGER NOT NULL,
                 ctx_start     INTEGER NOT NULL,
                 ctx_peak      INTEGER NOT NULL
             );",
        )
        .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let sql = declared_sql(&store.conn, "index").unwrap();
        assert!(sql["events_by_surface"].contains("surface_kind"), "{sql:?}");
    }

    #[test]
    fn a_catalog_without_the_startup_cost_column_is_rebuilt() {
        // A catalog weighed before startup cost was split out would silently
        // answer every "what does removing this save" question with nothing, so
        // it must not survive the upgrade as it is. `surfaces` is regenerated
        // from live config on every analyze, so reconciling it costs nothing to
        // drop — and clearing `analyzed_at` is what routes a `--frozen` read to
        // the freshness warning instead of reporting the empty catalog as a
        // finding.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE surfaces (kind TEXT NOT NULL, id TEXT NOT NULL,
                                    scope TEXT NOT NULL, project TEXT NOT NULL,
                                    static_tokens INTEGER, load_mode TEXT NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('analyzed_at', '2026-01-01T00:00:00Z');",
        )
        .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let names: Vec<String> = table_shape(&store.conn, "surfaces")
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert!(names.contains(&"startup_tokens".to_string()), "{names:?}");
        assert_eq!(store.meta("analyzed_at").unwrap(), None);
    }

    #[test]
    fn a_store_without_subagent_runs_gains_the_table_and_is_re_extracted() {
        // An existing store holds the events but not the runs, and its
        // transcripts have stopped changing — so nothing would ever populate the
        // new table, silently, since every query still succeeds and returns
        // nothing. Two mechanisms cover it, and neither alone would: `migrate`
        // supplies the table, and the analyzer version is what gets the rows
        // extracted into it.
        let mut store = Store::in_memory().unwrap();
        store
            .record_ingested_file("/tmp/example/s1.jsonl", 1, 2)
            .unwrap();
        store
            .conn
            .execute_batch("DROP TABLE subagent_runs")
            .unwrap();
        // Stamped by a build that predates subagent extraction.
        store.set_meta(TEST_ANALYZER_KEY, "0").unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        assert!(table_exists(&store.conn, "subagent_runs").unwrap());
        assert!(
            !store
                .is_analyzer_current(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
                .unwrap()
        );
    }

    #[test]
    fn reopening_a_current_store_keeps_its_fingerprints() {
        // Migration reconciles shape only. Re-running analyze against a store
        // the current analyzer already wrote must stay cheap.
        let mut store = Store::in_memory().unwrap();
        store
            .record_ingested_file("/tmp/example/s1.jsonl", 1, 2)
            .unwrap();
        store
            .record_analyzer_version(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
            .unwrap();
        let store = Store::from_connection(store.conn).unwrap();

        assert!(
            store
                .is_analyzer_current(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
                .unwrap()
        );
        assert!(store.is_ingested("/tmp/example/s1.jsonl", 1, 2).unwrap());
    }

    fn span_at_ctx(skill: &str, ctx_start: u64) -> Span {
        Span {
            skill: skill.to_string(),
            source: Source::Slash,
            started_epoch_ms: 1_700_000_000_000,
            duration_sec: 1.0,
            out_tokens: 10,
            ctx_growth: 5,
            ctx_start,
            ctx_peak: ctx_start,
            model: None,
            is_trailing: false,
            spawns: Vec::new(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    #[test]
    fn baseline_floor_is_the_leanest_session_start_per_project() {
        let mut store = Store::in_memory().unwrap();
        // Project "alpha" started two sessions at 30000 and 12000; "beta" one
        // at 40000.
        let mut alpha1 = session("a1");
        alpha1.project = "alpha".to_string();
        alpha1.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 30000,
        });
        store.ingest(IngestBatch::new(&alpha1, &[], &[])).unwrap();
        let mut alpha2 = session("a2");
        alpha2.project = "alpha".to_string();
        alpha2.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 12000,
        });
        store.ingest(IngestBatch::new(&alpha2, &[], &[])).unwrap();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        beta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 40000,
        });
        store.ingest(IngestBatch::new(&beta, &[], &[])).unwrap();

        assert_eq!(store.baseline_floor(TimeRange::ALL).unwrap(), Some(12000));
        assert_eq!(
            store.baseline_floor_per_project(TimeRange::ALL).unwrap(),
            vec![
                ("beta".to_string(), 40000, 1),
                ("alpha".to_string(), 12000, 2)
            ]
        );
    }

    #[test]
    fn baseline_floor_ignores_the_context_a_skill_happened_to_run_at() {
        let mut store = Store::in_memory().unwrap();
        // A session that started lean but only invoked a skill deep into a long
        // conversation. The span's ctx_start is not a floor observation.
        let mut meta = session("s1");
        meta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 50000,
        });
        store
            .ingest(IngestBatch::new(
                &meta,
                &[span_at_ctx("git-commit", 430000)],
                &[],
            ))
            .unwrap();

        assert_eq!(store.baseline_floor(TimeRange::ALL).unwrap(), Some(50000));
    }

    #[test]
    fn a_session_start_event_is_stamped_with_when_the_session_began() {
        // `sql` exposes raw event rows, so an unstamped row would place every
        // session start in 1970.
        let mut store = Store::in_memory().unwrap();
        let mut meta = session("s1");
        meta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 50000,
        });
        store.ingest(IngestBatch::new(&meta, &[], &[])).unwrap();

        let (_, rows) = store
            .query("SELECT started_epoch, started_at FROM events WHERE kind = 'session_start'")
            .unwrap();
        assert_eq!(rows[0][0], "1700000000");
        assert!(!rows[0][1].is_empty());
    }

    #[test]
    fn a_session_without_spans_still_records_when_it_started() {
        // started_at was derived only from spans, so a session that invoked no
        // skill was stored with an empty start.
        let mut store = Store::in_memory().unwrap();
        let mut meta = session("s1");
        meta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 50000,
        });
        store.ingest(IngestBatch::new(&meta, &[], &[])).unwrap();

        let (_, rows) = store
            .query("SELECT started_at FROM sessions WHERE id = 's1'")
            .unwrap();
        assert!(!rows[0][0].is_empty());
    }

    #[test]
    fn baseline_floor_is_absent_when_no_session_start_was_observed() {
        let mut store = Store::in_memory().unwrap();
        let meta = session("s1");
        store
            .ingest(IngestBatch::new(
                &meta,
                &[span_at_ctx("git-commit", 430000)],
                &[],
            ))
            .unwrap();

        assert_eq!(store.baseline_floor(TimeRange::ALL).unwrap(), None);
        assert_eq!(
            store.baseline_floor_per_project(TimeRange::ALL).unwrap(),
            vec![]
        );
    }

    #[test]
    fn always_on_config_sums_every_surface_startup_cost() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[
                Surface {
                    kind: "claude_md".to_string(),
                    id: "global".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/CLAUDE.md".to_string(),
                    static_tokens: Some(600),
                    startup_tokens: Some(600),
                    load_mode: LoadMode::StartupFull,
                },
                Surface {
                    kind: "rule".to_string(),
                    id: "git/safety".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/safety.md".to_string(),
                    static_tokens: Some(900),
                    startup_tokens: Some(900),
                    load_mode: LoadMode::StartupFull,
                },
                // A skill contributes its description, never its 1000-token body.
                skill_surface("git-commit", Scope::Global, 1000, 8),
                // An MCP server's schema is real but unweighable — it must not
                // land in the sum as a false zero-cost row either way.
                Surface {
                    kind: "mcp_server".to_string(),
                    id: "playwright".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/.mcp.json".to_string(),
                    static_tokens: None,
                    startup_tokens: None,
                    load_mode: LoadMode::ToolSchema,
                },
                // Another project's CLAUDE.md is always-on *there*, not globally.
                Surface {
                    kind: "claude_md".to_string(),
                    id: "project".to_string(),
                    scope: Scope::Project("alpha".to_string()),
                    config_path: "/tmp/example/CLAUDE.md".to_string(),
                    static_tokens: Some(400),
                    startup_tokens: Some(400),
                    load_mode: LoadMode::StartupFull,
                },
            ])
            .unwrap();

        // The global figure excludes project config; a session in alpha pays
        // the global floor plus alpha's own always-on config.
        assert_eq!(store.always_on_config_tokens().unwrap(), 1508);
        assert_eq!(store.always_on_config_tokens_for("alpha").unwrap(), 1908);
    }

    #[test]
    fn the_catalog_carries_startup_and_body_cost_apart() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[skill_surface("repro", Scope::Global, 1015, 7)])
            .unwrap();

        let catalog = store.effective_catalog(TimeRange::ALL).unwrap();
        assert_eq!(catalog[0].static_tokens, Some(1015));
        assert_eq!(catalog[0].startup_tokens, Some(7));
    }

    #[test]
    fn replace_surfaces_rebuilds_wholesale() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[surface("old", Scope::Global, 1)])
            .unwrap();
        store
            .replace_surfaces(&[surface("new", Scope::Global, 1)])
            .unwrap();

        let catalog = store.effective_catalog(TimeRange::ALL).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "new");
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let mut store = Store::in_memory().unwrap();
        assert_eq!(store.meta("analyzed_at").unwrap(), None);
        store
            .set_meta("analyzed_at", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .set_meta("analyzed_at", "2026-01-02T00:00:00Z")
            .unwrap();
        assert_eq!(
            store.meta("analyzed_at").unwrap().as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
    }

    #[test]
    fn an_unchanged_fingerprint_marks_a_file_ingested() {
        let mut store = Store::in_memory().unwrap();
        // Never seen: needs ingest.
        assert!(!store.is_ingested("/tmp/a.jsonl", 100, 5).unwrap());
        store.record_ingested_file("/tmp/a.jsonl", 100, 5).unwrap();
        // Same (mtime, size): skip. Any change: re-ingest.
        assert!(store.is_ingested("/tmp/a.jsonl", 100, 5).unwrap());
        assert!(!store.is_ingested("/tmp/a.jsonl", 101, 5).unwrap());
        assert!(!store.is_ingested("/tmp/a.jsonl", 100, 6).unwrap());
        // A new fingerprint supersedes the old one.
        store.record_ingested_file("/tmp/a.jsonl", 101, 6).unwrap();
        assert!(store.is_ingested("/tmp/a.jsonl", 101, 6).unwrap());
    }

    #[test]
    fn a_store_is_stale_until_stamped_with_the_current_analyzer() {
        let mut store = Store::in_memory().unwrap();
        // A store written by an older analyzer (or none at all) is stale, so
        // `analyze` re-ingests even the transcripts whose fingerprint matches.
        assert!(
            !store
                .is_analyzer_current(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
                .unwrap()
        );
        store.set_meta(TEST_ANALYZER_KEY, "0").unwrap();
        assert!(
            !store
                .is_analyzer_current(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
                .unwrap()
        );
        store
            .record_analyzer_version(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
            .unwrap();
        assert!(
            store
                .is_analyzer_current(TEST_ANALYZER_KEY, TEST_ANALYZER_VERSION)
                .unwrap()
        );
    }

    #[test]
    fn error_counts_break_down_by_project_and_category() {
        let mut store = Store::in_memory().unwrap();
        // Two projects, the same category concentrated in one of them.
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).tool_errors(&[
                (100, "edit-precondition", "x", "Edit", "f"),
                (200, "edit-precondition", "y", "Edit", "f"),
            ]))
            .unwrap();
        store
            .ingest(IngestBatch::new(&beta, &[], &[]).tool_errors(&[(
                300,
                "edit-precondition",
                "z",
                "Write",
                "f",
            )]))
            .unwrap();

        let rows = store.error_counts_by_project(TimeRange::ALL).unwrap();

        // Densest (project, category) pair first — alpha owns the friction.
        assert_eq!(
            rows,
            vec![
                ("alpha".to_string(), "edit-precondition".to_string(), 2),
                ("beta".to_string(), "edit-precondition".to_string(), 1),
            ]
        );
    }

    #[test]
    fn error_examples_are_capped_per_project_and_category() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).tool_errors(&[
                (100, "path-not-found", "missing /a", "Read", "f"),
                (200, "path-not-found", "missing /b", "Read", "f"),
                (300, "path-not-found", "missing /c", "Bash", "f"),
            ]))
            .unwrap();

        // Two examples per group, earliest first — the third is dropped.
        let examples = store.error_examples(2, TimeRange::ALL).unwrap();
        assert_eq!(
            examples,
            vec![
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "missing /a".to_string()
                ),
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "missing /b".to_string()
                ),
            ]
        );
    }

    #[test]
    fn session_stats_count_sessions_and_distinct_projects() {
        let mut store = Store::in_memory().unwrap();
        let mut a = session("a1");
        a.project = "alpha".to_string();
        let mut b = session("b1");
        b.project = "alpha".to_string();
        let mut c = session("c1");
        c.project = "beta".to_string();
        store.ingest(IngestBatch::new(&a, &[], &[])).unwrap();
        store.ingest(IngestBatch::new(&b, &[], &[])).unwrap();
        store.ingest(IngestBatch::new(&c, &[], &[])).unwrap();

        assert_eq!(store.session_stats(TimeRange::ALL).unwrap(), (3, 2));
    }

    #[test]
    fn work_events_and_tool_errors_carry_a_readable_timestamp() {
        // The epoch is not enough for a sql read: an advisor drilling into a
        // session wants the wall-clock time without re-deriving it, and the
        // headless walkthrough stalled on the empty column.
        let mut store = Store::in_memory().unwrap();
        let a1 = session("a1");
        store
            .ingest(
                IngestBatch::new(&a1, &[], &[])
                    .tool_errors(&[(
                        2_000,
                        "path-not-found",
                        "no such file",
                        "read_file",
                        "/repo/src/y.rs",
                    )])
                    .work_events(&[(1_000, "file_edit", "x.rs", Some("/repo/src/x.rs"))]),
            )
            .unwrap();

        let rows = store
            .query("SELECT kind, started_at FROM events ORDER BY started_epoch")
            .unwrap()
            .1;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], "1970-01-01T00:00:01+00:00");
        assert_eq!(rows[1][1], "1970-01-01T00:00:02+00:00");
    }

    #[test]
    fn file_edits_carry_their_project_session_and_full_path() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).work_events(&[(
                1_000,
                "file_edit",
                "x.rs",
                Some("/repo/src/x.rs"),
            )]))
            .unwrap();

        assert_eq!(
            store.file_edits(TimeRange::ALL).unwrap(),
            vec![FileEdit {
                project: "alpha".to_string(),
                session_id: "a1".to_string(),
                path: "/repo/src/x.rs".to_string(),
                epoch: 1,
            }]
        );
    }

    #[test]
    fn a_file_edit_without_a_recorded_path_is_excluded() {
        // Falling back to the basename would silently regroup same-named files —
        // the very merge this query exists to prevent — for every store written
        // before paths were recorded.
        let mut store = Store::in_memory().unwrap();
        let alpha = session("a1");
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).work_events(&[(
                1_000,
                "file_edit",
                "x.rs",
                None,
            )]))
            .unwrap();

        assert_eq!(store.file_edits(TimeRange::ALL).unwrap(), vec![]);
    }

    #[test]
    fn query_json_preserves_sqlite_types() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest(IngestBatch::new(&alpha, &[], &[])).unwrap();

        let (cols, rows) = store
            .query_json("SELECT project, COUNT(*) AS n, NULL AS absent, 1.5 AS ratio FROM sessions")
            .unwrap();

        assert_eq!(cols, vec!["project", "n", "absent", "ratio"]);
        // Numbers stay numbers, NULL stays null — a `--format json` consumer
        // must not need to re-parse strings.
        assert_eq!(
            rows,
            vec![vec![
                serde_json::json!("alpha"),
                serde_json::json!(1),
                serde_json::Value::Null,
                serde_json::json!(1.5),
            ]]
        );
    }

    #[test]
    fn query_returns_columns_and_stringified_rows() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).tool_errors(&[(
                100,
                "path-not-found",
                "missing /a",
                "Read",
                "f",
            )]))
            .unwrap();

        let (cols, rows) = store
            .query("SELECT category, tool, COUNT(*) AS n FROM tool_errors GROUP BY category, tool")
            .unwrap();
        assert_eq!(cols, vec!["category", "tool", "n"]);
        assert_eq!(
            rows,
            vec![vec![
                "path-not-found".to_string(),
                "Read".to_string(),
                "1".to_string()
            ]]
        );
    }

    #[test]
    fn tool_errors_view_exposes_named_columns_and_project() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "demo--wt-feature".to_string();
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).tool_errors(&[(
                100,
                "path-not-found",
                "missing /a",
                "Read",
                "f",
            )]))
            .unwrap();

        // An ad-hoc query against the view sees clean names, not the overloaded
        // event columns, and the worktree filter works off `project`.
        let row: (String, String, String, String) = store
            .conn
            .query_row(
                "SELECT project, category, excerpt, tool FROM tool_errors \
                 WHERE project LIKE '%--wt%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "demo--wt-feature".to_string(),
                "path-not-found".to_string(),
                "missing /a".to_string(),
                "Read".to_string(),
            )
        );
    }

    #[test]
    fn error_tool_breakdown_splits_a_category_across_tools() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store
            .ingest(IngestBatch::new(&alpha, &[], &[]).tool_errors(&[
                (100, "path-not-found", "missing /a", "Read", "f"),
                (200, "path-not-found", "missing /b", "Read", "f"),
                (300, "path-not-found", "missing /c", "Bash", "f"),
            ]))
            .unwrap();

        // path-not-found splits Read 2 / Bash 1, densest tool first.
        let rows = store.error_tool_breakdown(TimeRange::ALL).unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "Read".to_string(),
                    2
                ),
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "Bash".to_string(),
                    1
                ),
            ]
        );
    }

    // ---- PBI 02: batch ingest order, typed open guarantees, encoding table ----

    fn dump(store: &Store, table: &str) -> Vec<Vec<String>> {
        store
            .query(&format!("SELECT * FROM {table} ORDER BY id"))
            .unwrap()
            .1
    }

    /// Ingest the batch a caller would submit for a fresh read of one file.
    fn ingest_fresh(store: &mut Store, meta: &SessionMeta) {
        store
            .ingest(
                IngestBatch::new(
                    meta,
                    &[span("new-skill", 7, 7, 2.0)],
                    &[agent_spawn("agent", "toolu_new")],
                )
                .prompts(&[(9, 1_700_000_100_000, "instruction")])
                .subagent_runs(&[subagent_run("Explore", "new", "toolu_new", 222)])
                .tool_errors(&[(2_000, "edit-precondition", "new", "Edit", "f")])
                .work_events(&[(
                    2_000,
                    "file_edit",
                    "new.rs",
                    Some("/repo/new.rs"),
                )]),
            )
            .unwrap();
    }

    #[test]
    fn a_batch_replaces_every_prior_row_for_the_file_wholesale() {
        // Order-independence the old five-function API could not express: a
        // store that previously held rows of every kind for this file must end
        // up byte-identical to a store that only ever saw the fresh batch.
        let mut seen = Store::in_memory().unwrap();
        let meta = session("s1");
        seen.ingest(
            IngestBatch::new(
                &meta,
                &[span("old-skill", 5, 5, 1.0)],
                &[agent_spawn("agent", "toolu_old")],
            )
            .prompts(&[(3, 1_700_000_000_000, "instruction")])
            .subagent_runs(&[subagent_run("Explore", "old", "toolu_old", 111)])
            .tool_errors(&[(1_000, "path-not-found", "old", "Read", "f")])
            .work_events(&[(1_000, "file_edit", "old.rs", Some("/repo/old.rs"))]),
        )
        .unwrap();
        ingest_fresh(&mut seen, &meta);

        let mut fresh = Store::in_memory().unwrap();
        ingest_fresh(&mut fresh, &meta);

        for table in ["sessions", "events", "subagent_runs"] {
            assert_eq!(
                dump(&seen, table),
                dump(&fresh, table),
                "{table} must not depend on what was ingested before"
            );
        }
    }

    #[test]
    fn one_batch_settles_the_spawn_costs_of_the_runs_it_carries() {
        // The spawn-cost settle used to run in a separate call after
        // `ingest_session`; inside a batch it must settle in the same commit.
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store
            .ingest(
                IngestBatch::new(&s1, &[], &[agent_spawn("Explore", "toolu_a1")])
                    .subagent_runs(&[subagent_run("Explore", "a1", "toolu_a1", 400)]),
            )
            .unwrap();

        let rows = store
            .query("SELECT out_tokens FROM events WHERE kind = 'agent_spawn'")
            .unwrap()
            .1;
        assert_eq!(rows, vec![vec!["400".to_string()]]);
    }

    #[test]
    fn readonly_open_neither_creates_nor_migrates() {
        // The two guarantees `open`/`open_readonly` used to bury in their names
        // are now the `OpenMode` argument's contract.
        let dir =
            std::env::temp_dir().join(format!("saniwalens-readonly-test-{}", std::process::id()));
        let path = dir.join("store.db");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            Store::open(&path, OpenMode::ReadOnly).is_err(),
            "a missing store is an error, never a fresh empty one"
        );
        assert!(!path.exists(), "readonly must not create the file");

        // An old-shape store opens readonly as-is: no migration runs, so the
        // missing column surfaces as SQLite's own error.
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id          TEXT PRIMARY KEY,
                 project     TEXT NOT NULL,
                 slug        TEXT NOT NULL,
                 source_path TEXT NOT NULL,
                 started_at  TEXT NOT NULL
             );",
        )
        .unwrap();
        drop(conn);
        {
            let store = Store::open(&path, OpenMode::ReadOnly).unwrap();
            assert!(
                store.query("SELECT root FROM sessions").is_err(),
                "readonly must not migrate a legacy store"
            );
        }
        {
            let store = Store::open(&path, OpenMode::ReadWrite).unwrap();
            assert!(
                store.query("SELECT root FROM sessions").is_ok(),
                "ReadWrite is the migrating mode"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tool_errors_view_is_derived_from_the_encoding_table() {
        // Every meaning the view names must come from EVENT_ENCODING, not from
        // hand-written SQL that can drift away from it.
        let sql = tool_errors_view_sql(EVENT_ENCODING);
        let (_, roles) = EVENT_ENCODING
            .iter()
            .find(|(kind, _)| *kind == "tool_error")
            .expect("tool_error is in the encoding table");
        // Collapse padding so the assertion checks which column feeds which
        // alias, not how the generated text is laid out.
        let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        for (role, column) in *roles {
            assert!(
                flat.contains(&format!("e.{column} AS {role}")),
                "view misses {role} → {column}: {sql}"
            );
        }
        assert!(flat.contains("WHERE e.kind = 'tool_error'"));
    }

    #[test]
    fn adding_a_kind_to_the_encoding_table_leaves_the_view_untouched() {
        // A new event kind is a row in the table — existing view definitions
        // and readers must not need to change for it.
        let mut extended: Vec<(&str, &[(EventRole, EventColumn)])> = EVENT_ENCODING.to_vec();
        extended.push((
            "brand_new_kind",
            &[(EventRole::Category, EventColumn::Target)],
        ));

        assert_eq!(
            tool_errors_view_sql(EVENT_ENCODING),
            tool_errors_view_sql(&extended),
            "the view is defined only by the tool_error entry"
        );
    }

    // ---- PBI 2026-10-30-01: the report views' evaluation window ----

    fn span_at(skill: &str, epoch_secs: i64, out_tokens: u64) -> Span {
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

    #[test]
    fn a_range_narrows_event_aggregates_at_the_inclusive_bounds() {
        let mut store = Store::in_memory().unwrap();
        let s = session("s1");
        let spans = [
            span_at("commit", 1_000, 100),
            span_at("commit", 2_000, 200),
            span_at("commit", 3_000, 400),
        ];
        store.ingest(IngestBatch::new(&s, &spans, &[])).unwrap();

        // Both ends are inclusive: a one-second window catches the span that
        // starts at it, and nothing else.
        let rollup = store
            .skill_usage(TimeRange {
                start: Some(2_000),
                end: Some(2_000),
            })
            .unwrap();
        assert_eq!(rollup.len(), 1);
        assert_eq!(rollup[0].invocations, 1);
        assert_eq!(rollup[0].out_tokens, 200);

        // Without the period flags the whole spine comes back, unchanged.
        assert_eq!(store.skill_usage(TimeRange::ALL).unwrap()[0].invocations, 3);
    }

    #[test]
    fn a_range_narrows_session_aggregates_by_the_stamped_utc_text() {
        let mut store = Store::in_memory().unwrap();
        // Last second of JST 2026-10-07: local midnight of the 8th is
        // 2026-10-07T15:00:00Z.
        let end = 1_791_385_200 - 1;
        // A session that started at 23:59:59.5 JST on the 7th — the millisecond
        // fraction sits inside the final second, so it stays in the window.
        let mut inside = session("inside");
        inside.sub_tokens = 400;
        inside.sub_agent_count = 1;
        inside.start = Some(SessionStart {
            timestamp_ms: end * 1000 + 500,
            ctx: 10_000,
        });
        store.ingest(IngestBatch::new(&inside, &[], &[])).unwrap();
        // A session that started at local midnight of the 8th — outside.
        let mut outside = session("outside");
        outside.sub_tokens = 500;
        outside.sub_agent_count = 1;
        outside.start = Some(SessionStart {
            timestamp_ms: (end + 1) * 1000,
            ctx: 11_000,
        });
        store.ingest(IngestBatch::new(&outside, &[], &[])).unwrap();

        let range = TimeRange {
            start: None,
            end: Some(end),
        };
        assert_eq!(store.subagent_totals(range).unwrap(), (400, 1));
        assert_eq!(store.session_stats(range).unwrap().0, 1);
        // Without the flags both sessions count.
        assert_eq!(store.subagent_totals(TimeRange::ALL).unwrap(), (900, 2));
    }

    #[test]
    fn inventory_narrows_the_usage_half_and_keeps_the_catalog_rows() {
        let mut store = Store::in_memory().unwrap();
        let s = session("s1");
        let spans = [span_at("commit", 1_000, 100)];
        let calls = [UsageEvent {
            surface_kind: "agent".to_string(),
            surface_id: "Task".to_string(),
            started_epoch_ms: 2_000_000,
            tool_use_id: None,
        }];
        store.ingest(IngestBatch::new(&s, &spans, &calls)).unwrap();
        store
            .replace_surfaces(&[
                skill_surface("commit", Scope::Global, 10, 5),
                surface("Task", Scope::Global, 20),
            ])
            .unwrap();

        // The span starts at 1_000, the agent call at 2_000: a window ending
        // before the call narrows the usage half only — the catalog (current
        // config) keeps both rows.
        let catalog = store
            .effective_catalog(TimeRange {
                start: None,
                end: Some(1_500),
            })
            .unwrap();
        assert_eq!(catalog.len(), 2, "catalog rows never disappear");
        let skill = catalog
            .iter()
            .find(|e| e.kind == "skill" && e.id == "commit")
            .unwrap();
        assert_eq!(skill.uses, 1, "the span is inside the window");
        let task = catalog
            .iter()
            .find(|e| e.kind == "skill" && e.id == "Task")
            .unwrap();
        assert_eq!(task.uses, 0, "the call is outside the window");
    }

    #[test]
    fn a_range_narrows_failure_counts_and_work_events() {
        let mut store = Store::in_memory().unwrap();
        let mut s1 = session("s1");
        s1.start = Some(SessionStart {
            timestamp_ms: 500_000,
            ctx: 1_000,
        });
        let errors = [(1_000_000, "path-not-found", "no /x", "Read", "/x")];
        let edits = [
            (2_000_000, "file_edit", "main.rs", Some("/repo/main.rs")),
            (3_000_000, "file_edit", "lib.rs", Some("/repo/lib.rs")),
        ];
        store
            .ingest(
                IngestBatch::new(&s1, &[], &[])
                    .tool_errors(&errors)
                    .work_events(&edits),
            )
            .unwrap();

        let range = TimeRange {
            start: Some(2_500),
            end: None,
        };
        assert!(store.error_counts(range).unwrap().is_empty());
        let edits = store.file_edits(range).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "/repo/lib.rs");
        // Unbounded, everything comes back.
        assert_eq!(store.error_counts(TimeRange::ALL).unwrap().len(), 1);
        assert_eq!(store.file_edits(TimeRange::ALL).unwrap().len(), 2);
    }
}
