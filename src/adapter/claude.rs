//! Claude Code's analyze routine: discovers transcripts and config under the
//! Claude Code roots, extracts them, and hands the rows to the store. The
//! seam is `Harness::analyze` (`harness.rs`) — this module is Claude's half
//! of it (`docs/specs/harness.md`, `session-format.md`, `config-format.md`).
//!
//! It is the one place that walks the Claude Code input route (the projects
//! directory), the per-transcript fingerprint and slug folding rules, and the
//! global/project config catalog that `analyze` stamps into the store.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::adapter::config::{
    claude_home, read_agent_surfaces, read_claude_md_surfaces, read_mcp_server_surfaces,
    read_project_surfaces, read_rule_surfaces, read_skill_surfaces,
};
use crate::adapter::harness::{AnalyzeStats, Harness, file_fingerprint, home_dir};
use crate::adapter::transcript::{
    CLAUDE_ANALYZER_META_KEY, CLAUDE_ANALYZER_VERSION, count_permission_denials,
    extract_prompt_pointers, extract_tool_errors, extract_work_events, parse_session,
    parse_subagent_sidecar, session_cwd, subagent_id_from_file_name, subagent_prompt_id,
    subagent_sidecar_path, subagents_dir,
};
use crate::core::span::{DEFAULT_IDLE_GAP_MS, SessionStart, extract_spans, session_start};
use crate::core::subagent::{SubagentRun, link_runs};
use crate::core::surface::{Scope, Surface};
use crate::core::usage::{attribute_subagents, extract_usage_events, output_tokens};
use crate::store::{IngestBatch, OpenMode, SessionMeta, Store};

fn default_projects_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("projects"))
}

/// Fold a worktree slug onto its parent project so usage is not split across
/// `...--wt-feat-x` directories (see `docs/specs/storage.md`).
fn normalize_project(slug: &str) -> String {
    match slug.split_once("--wt-") {
        Some((parent, _)) => parent.to_string(),
        None => slug.to_string(),
    }
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

pub fn analyze(projects: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    let projects = projects.map(Ok).unwrap_or_else(default_projects_dir)?;
    let mut store = Store::open(db, OpenMode::ReadWrite).context("open store")?;

    // Rows produced by older analysis semantics are rebuilt once: the
    // fingerprint skip below would otherwise pin them forever, because the
    // transcripts they came from will never change again.
    let reuse_ingested =
        store.is_analyzer_current(CLAUDE_ANALYZER_META_KEY, CLAUDE_ANALYZER_VERSION)?;

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
        let prompts: Vec<(usize, i64, &str)> = extract_prompt_pointers(&text)
            .into_iter()
            .map(|(line, ts, behavior)| (line, ts, behavior.label()))
            .collect();
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
        // `work` owns the strings; the rows borrow them.
        let work = extract_work_events(&text);
        let work_rows: Vec<(i64, &str, &str, Option<&str>)> = work
            .iter()
            .map(|e| (e.epoch_ms, e.kind, e.id.as_str(), e.path.as_deref()))
            .collect();
        // One transcript, one batch: the store applies its parts in its own
        // order, so nothing here depends on the order these were extracted in.
        store.ingest(
            IngestBatch::new(&meta, &spans, &usage)
                .subagent_runs(&runs)
                .prompts(&prompts)
                .tool_errors(&error_rows)
                .work_events(&work_rows),
        )?;
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

    store.record_analyzer_version(CLAUDE_ANALYZER_META_KEY, CLAUDE_ANALYZER_VERSION)?;
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// One synthetic transcript and one synthetic skill under a fixture config
    /// root — never a real transcript
    /// (`.claude/rules/session-data-privacy.md`).
    ///
    /// The expected counts are what the **pre-refactor binary** produced for
    /// exactly this fixture (recorded 2026-10-30): `analyzed 1 session(s)
    /// (0 unchanged), 1 skill invocation(s), 1 surface(s) catalogued,
    /// 0 subagent tokens across 0 subagent(s), 0 permission denial(s)`.
    fn fixture(dir: &Path) {
        let projects = dir.join("projects/-tmp-example-proj");
        let skill = dir.join("skills/demo-skill");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        let jsonl = concat!(
            r#"{"type":"user","cwd":"/tmp/example/proj","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"<command-name>/git-commit</command-name>"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":0,"output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03.000Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":0,"cache_read_input_tokens":250,"cache_creation_input_tokens":0,"output_tokens":60}}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:05.000Z","message":{"content":"thanks"}}"#,
        );
        std::fs::write(
            projects.join("aaaaaaaa-0000-0000-0000-000000000001.jsonl"),
            jsonl,
        )
        .unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: Demo skill for the analyze fixture\n---\n# demo\n",
        )
        .unwrap();
    }

    #[test]
    fn claude_analyze_keeps_the_recorded_counts_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!(
            "saniwalens-claude-analyze-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        // The routine resolves its default input route and config root from
        // the environment, exactly as before the seam existed; this is the
        // only test that sets `CLAUDE_CONFIG_DIR`, so the env read has no
        // parallel reader to race with.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

        let db = dir.join("store.db");
        let stats = crate::adapter::harness::Harness::Claude
            .analyze(None, &db)
            .unwrap();

        assert_eq!(
            (
                stats.sessions,
                stats.skipped,
                stats.spans_total,
                stats.surface_count,
                stats.denials
            ),
            (1, 0, 1, 1, 0),
            "the counts the pre-refactor analyze produced for this fixture"
        );

        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
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
    fn normalize_root_folds_a_worktree_path_onto_its_repo() {
        assert_eq!(
            normalize_root("/tmp/example/repo/.wt/feat-x"),
            "/tmp/example/repo"
        );
        // Idempotent: an already-folded root stays put.
        assert_eq!(normalize_root("/tmp/example/repo"), "/tmp/example/repo");
    }
}
