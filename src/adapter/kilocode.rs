//! Parse kilocode session data (the two JSON files under a task directory)
//! into domain records.
//!
//! kilocode splits what other harnesses keep in one place: the conversation
//! (`api_conversation_history.json`) records what the model did but carries
//! **no usage**, and the display log (`ui_messages.json`) carries the usage
//! but no structured conversation. `parse_session` joins the two in time
//! order — one `Assistant` record per `api_req_started` row, its tokens
//! attached to the window of assistant messages the row opens. The function
//! is pure over the two files' JSON text; reading the task directory is a
//! thin shell around it. See `docs/specs/session-format-kilocode.md`. Like
//! every adapter it deserializes defensively: only the needed fields, unknown
//! kinds ignored, missing optional ones tolerated.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::adapter::harness::{
    AnalyzeStats, Harness, Site, catalog_surfaces, file_fingerprint, home_dir, load_agents,
    load_claude_md, load_commands, load_kilo_instructions, load_skills_deduped,
};
use crate::adapter::transcript::{ToolError, WorkEvent};
use crate::core::friction::classify_error;
use crate::core::path::basename;
use crate::core::span::{DEFAULT_IDLE_GAP_MS, Record, RecordKind, extract_spans, session_start};
use crate::core::surface::{Scope, Surface};
use crate::core::usage::extract_usage_events;
use crate::store::{IngestBatch, OpenMode, SessionMeta, Store};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The model label when a payload names no model — honest, never a guess.
/// kilocode's payload carries `inferenceProvider` only: a provider is never a
/// model, so mapping one to the other would be a guess.
const UNKNOWN_MODEL: &str = "unknown";

/// The analysis semantics the kilocode mapping was built with. **Bump it
/// whenever kilocode extraction changes what unchanged files yield.** It
/// lives apart from the shared `CLAUDE_ANALYZER_VERSION` (`adapter::transcript`) on purpose:
/// a kilocode-only change must never force other harnesses through a full
/// re-analyze. Stored under its own `meta` key.
pub const KILOCODE_ANALYZER_VERSION: &str = "3";

/// The `meta` key holding the kilocode analysis semantics.
pub const KILOCODE_ANALYZER_META_KEY: &str = "kilocode_analyzer_version";

/// One `api_req_started` row's usage payload. A row whose payload is missing,
/// unparseable, or marked `usageMissing=true` still happened — it carries
/// zeros, never a fabricated measurement.
struct RequestUsage {
    tokens_in: u64,
    tokens_out: u64,
    cache_reads: u64,
    cache_writes: u64,
}

/// Parse one session's two files into domain records, in `ts` order.
///
/// Each `api_req_started` row claims the assistant conversation messages
/// strictly between it and the next row — its *request window* — and yields
/// **one** `Assistant` record, never one per assistant message (one window can
/// hold several). The record stamps the window's *last* assistant message —
/// the request's completion point — so the cost record sits after its window's
/// tool records instead of inverting the stream; a window holding no assistant
/// message stamps the row's own `ts`. Assistant messages outside any window
/// carry no cost record: no measured request row, never fabricated.
pub fn parse_session(conversation: &str, ui_messages: &str) -> Vec<Record> {
    let Ok(parsed) = serde_json::from_str::<Value>(conversation) else {
        return Vec::new();
    };
    let Some(messages) = parsed.as_array() else {
        return Vec::new();
    };
    let ui = serde_json::from_str::<Value>(ui_messages).ok();
    let ui_rows: &[Value] = ui
        .as_ref()
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // The token source: one row per request, in `ts` order.
    let mut starts: Vec<(i64, RequestUsage)> = ui_rows
        .iter()
        .filter_map(|row| {
            if row.get("say").and_then(Value::as_str) != Some("api_req_started") {
                return None;
            }
            let ts = json_ts(row)?;
            Some((ts, request_usage(row)))
        })
        .collect();
    starts.sort_by_key(|(ts, _)| *ts);

    // The conversation spine: human turns, tool uses, and the assistant
    // timestamps the windows claim. Records carry a tie-breaker so the tool
    // records come before a cost record they share a `ts` with.
    let mut records: Vec<(i64, u8, RecordKind)> = Vec::new();
    let mut assistant_ts: Vec<i64> = Vec::new();
    let mut ordered: Vec<(&Value, i64)> = messages
        .iter()
        .filter_map(|message| json_ts(message).map(|ts| (message, ts)))
        .collect();
    ordered.sort_by_key(|(_, ts)| *ts); // stable: equal `ts` keep file order
    for (message, ts) in ordered {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" => {
                if has_text_material(message.get("content")) {
                    records.push((ts, 0, RecordKind::HumanTurn));
                }
            }
            "assistant" => {
                assistant_ts.push(ts);
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue; // text and unknown block types are display only
                        }
                        if let Some(tool) = block.get("name").and_then(Value::as_str) {
                            records.push((ts, 0, RecordKind::ToolUse { tool: tool.into() }));
                        }
                    }
                }
            }
            _ => {} // unknown roles ignored
        }
    }

    // The join: one cost record per request row, stamped at its window's end.
    for (index, (start_ts, usage)) in starts.iter().enumerate() {
        let next_ts = starts.get(index + 1).map(|(ts, _)| *ts).unwrap_or(i64::MAX);
        let stamp = assistant_ts
            .iter()
            .rev() // the window's last assistant message
            .find(|ts| **ts > *start_ts && **ts < next_ts)
            .copied()
            .unwrap_or(*start_ts);
        records.push((
            stamp,
            1,
            RecordKind::Assistant {
                prompt_size: usage.tokens_in + usage.cache_reads + usage.cache_writes,
                out_tokens: usage.tokens_out,
                model: UNKNOWN_MODEL.into(),
            },
        ));
    }

    records.sort_by_key(|(ts, tie, _)| (*ts, *tie));
    records
        .into_iter()
        .map(|(timestamp_ms, _, kind)| Record { timestamp_ms, kind })
        .collect()
}

/// Whether a message's content carries text material — the human-turn signal.
/// A bare string is tolerated as text; a `tool_result`-only user message (the
/// Anthropic-format mirror of a tool result) is not a human turn.
fn has_text_material(content: Option<&Value>) -> bool {
    match content {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("text")),
        _ => false,
    }
}

/// The `ts` both files stamp every row with: milliseconds since the epoch.
/// A float is tolerated — a missing value is not, it cannot be ordered.
fn json_ts(row: &Value) -> Option<i64> {
    row.get("ts")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

/// One row's usage payload. `usageMissing=true` means no tokens; a missing or
/// unparseable payload has no numbers to read, which is zeros either way.
fn request_usage(row: &Value) -> RequestUsage {
    let payload = row
        .get("text")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let number = |key: &str| {
        payload
            .as_ref()
            .and_then(|p| p.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let usage_missing = payload
        .as_ref()
        .and_then(|p| p.get("usageMissing"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (tokens_in, tokens_out, cache_reads, cache_writes) = if usage_missing {
        (0, 0, 0, 0)
    } else {
        (
            number("tokensIn"),
            number("tokensOut"),
            number("cacheReads"),
            number("cacheWrites"),
        )
    };
    RequestUsage {
        tokens_in,
        tokens_out,
        cache_reads,
        cache_writes,
    }
}

/// One failed tool call per `tool_result` flagged `is_error`: its friction
/// category, a readable excerpt, the originating tool, and the call's target
/// from its input. `tool_use_id` pairs the failure back to the exact
/// `tool_use` block — the same join Claude Code uses; a call with no match
/// names the tool `unknown` and keeps its excerpt intact.
pub fn extract_tool_errors(conversation: &str) -> Vec<ToolError> {
    let Ok(parsed) = serde_json::from_str::<Value>(conversation) else {
        return Vec::new();
    };
    let Some(messages) = parsed.as_array() else {
        return Vec::new();
    };

    // tool_use id -> (tool name, target), filled as the assistant messages
    // stream past (a tool_use always precedes its result, so one forward pass
    // suffices).
    let mut tool_calls: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut errors = Vec::new();
    let mut ordered: Vec<(&Value, i64)> = messages
        .iter()
        .filter_map(|message| json_ts(message).map(|ts| (message, ts)))
        .collect();
    ordered.sort_by_key(|(_, ts)| *ts);
    for (message, ts) in ordered {
        match message.get("role").and_then(Value::as_str).unwrap_or("") {
            "assistant" => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    ) {
                        let target = tool_target(block.get("input"));
                        tool_calls.insert(id.to_string(), (name.to_string(), target));
                    }
                }
            }
            "user" => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result")
                        || block.get("is_error").and_then(Value::as_bool) != Some(true)
                    {
                        continue;
                    }
                    let content = block.get("content");
                    let call = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .and_then(|id| tool_calls.get(id));
                    let (tool, target) = match call {
                        Some((name, target)) => (name.clone(), target.clone()),
                        None => ("unknown".into(), String::new()),
                    };
                    let text = content.map(error_text).unwrap_or_default();
                    errors.push(ToolError {
                        epoch_ms: ts,
                        // Classified on the same readable text the excerpt
                        // carries: kilocode's envelope wraps the message, and
                        // the envelope's own fields matched the wrong
                        // vocabulary ("timeout" among them).
                        category: classify_error(&text),
                        excerpt: truncate(&text),
                        tool,
                        target,
                    });
                }
            }
            _ => {} // unknown roles ignored
        }
    }
    errors
}

/// The subject of a tool call: the first present identifying input field.
/// kilocode spells the file key `path` and the command key `command`.
fn tool_target(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    for key in ["path", "file_path", "command", "pattern", "url"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return truncate(value);
        }
    }
    String::new()
}

/// The failure's readable excerpt: the string content, or the text blocks of
/// the array content, whitespace-collapsed and truncated by Unicode scalar.
/// The failure's readable text: for kilocode, each text block may be a JSON
/// envelope around the message — the measured shape is an envelope block
/// followed by an environment_details block, and joining the block texts made
/// the joined string non-JSON, so the blob was stored whole. Unwrapping per
/// block keeps the message and drops the envelope's own fields, which
/// mis-classified the failure ("does not exist" matched the path vocabulary,
/// "timeout" matched timeout). Claude-shaped plain text passes through.
fn error_text(content: &Value) -> String {
    match content {
        Value::String(text) => unwrap_envelope(text),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            // kilo decorates every tool_result with an environment_details
            // block — the editor's state, not the failure's cause. Its file
            // names matched the vocabulary ("timeout" in a test name) and
            // mis-classified the failure.
            .filter(|text| !text.trim_start().starts_with("<environment_details>"))
            .map(unwrap_envelope)
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

fn unwrap_envelope(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|envelope| {
            envelope
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.to_string())
}

/// Whitespace-collapse and truncate by Unicode scalar (never mid-character).
fn truncate(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

/// Estimate the session root. The strongest signal is the `cwd` kilocode
/// carries on every `execute_command` input: the first absolute value is the
/// root, the same confidence as Claude Code's first-`cwd` field (verified:
/// every task that carries a cwd names exactly one distinct value, while tool
/// paths are overwhelmingly relative). A task with no absolute cwd falls back
/// to the leading tool paths: the first two seed it, every later path under it
/// refines it, and the first path outside it ends the run (a later outlier
/// does not drag the estimate down). `None` when neither appears — the session
/// is then unrooted, never guessed.
pub fn estimate_root(conversation: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(conversation).ok()?;
    let messages = parsed.as_array()?;
    let mut ordered: Vec<(&Value, i64)> = messages
        .iter()
        .filter_map(|message| json_ts(message).map(|ts| (message, ts)))
        .collect();
    ordered.sort_by_key(|(_, ts)| *ts);

    let mut first_cwd: Option<String> = None;
    let mut parents: Vec<Vec<String>> = Vec::new();
    let mut full_paths: Vec<Vec<String>> = Vec::new();
    for (message, _) in ordered {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(input) = block.get("input") else {
                continue;
            };
            if first_cwd.is_none()
                && block.get("name").and_then(Value::as_str) == Some("execute_command")
            {
                let absolute = input
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|cwd| cwd.starts_with('/'));
                if let Some(cwd) = absolute {
                    first_cwd = Some(cwd.to_string());
                }
            }
            for path in input_paths(input) {
                if !path.starts_with('/') {
                    continue; // a relative path carries no root signal
                }
                let components: Vec<String> = path
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .map(String::from)
                    .collect();
                if components.len() < 2 {
                    continue; // a path directly under the filesystem root names no project
                }
                full_paths.push(components.clone());
                parents.push(components[..components.len() - 1].to_vec());
            }
        }
    }

    // The first absolute `cwd` wins outright — it is the start working
    // directory, not a guess from the files the task happened to touch.
    if let Some(cwd) = first_cwd {
        return Some(cwd);
    }

    let mut root: Vec<String> = match parents.as_slice() {
        [] => return None,
        [first] => first.clone(),
        [first, second, ..] => common_prefix(first, second),
    };
    for (index, parent) in parents.iter().enumerate().skip(2) {
        if !full_paths[index].starts_with(&root) {
            break; // the leading run ends here
        }
        root = common_prefix(&root, parent);
    }
    Some(format!("/{}", root.join("/")))
}

/// The absolute paths one tool input names: its `path` (string) and `files`
/// (array of strings). Relative paths are kept out by the caller's check.
fn input_paths(input: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = input.get("path").and_then(Value::as_str) {
        paths.push(path.to_string());
    }
    if let Some(files) = input.get("files").and_then(Value::as_array) {
        paths.extend(files.iter().filter_map(Value::as_str).map(String::from));
    }
    paths
}

/// The longest prefix two component lists share.
fn common_prefix(first: &[String], second: &[String]) -> Vec<String> {
    first
        .iter()
        .zip(second.iter())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.clone())
        .collect()
}

/// One unit of work kilocode performed: a command's leading word or an edit
/// target's basename (with the full path alongside, as for Claude Code work
/// events). Reads are not work — they change nothing.
pub fn extract_work_events(conversation: &str) -> Vec<WorkEvent> {
    let Ok(parsed) = serde_json::from_str::<Value>(conversation) else {
        return Vec::new();
    };
    let Some(messages) = parsed.as_array() else {
        return Vec::new();
    };

    let mut events = Vec::new();
    let mut ordered: Vec<(&Value, i64)> = messages
        .iter()
        .filter_map(|message| json_ts(message).map(|ts| (message, ts)))
        .collect();
    ordered.sort_by_key(|(_, ts)| *ts);
    for (message, ts) in ordered {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let input = block.get("input");
            match name {
                "execute_command" => {
                    if let Some(cmd) = input
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                        .and_then(|command| command.split_whitespace().next())
                    {
                        events.push(WorkEvent {
                            epoch_ms: ts,
                            kind: "bash_cmd",
                            id: cmd.to_string(),
                            path: None,
                        });
                    }
                }
                "edit_file" | "apply_diff" | "search_and_replace" | "write_to_file" => {
                    let path = tool_target(input);
                    if !path.is_empty() {
                        events.push(WorkEvent {
                            epoch_ms: ts,
                            kind: "file_edit",
                            id: basename(&path).to_string(),
                            path: Some(path),
                        });
                    }
                }
                _ => {} // reads and protocol tools are not work
            }
        }
    }
    events
}

/// The kilocode global config sites, as data (`adapter::harness::Site`).
pub(crate) fn global_sites(config_dir: &Path, home: Option<&Path>) -> Vec<Site> {
    let mut sites = vec![
        Site::new(config_dir.join("command"), load_commands),
        Site::new(config_dir.join("agent"), load_agents),
        Site::new(config_dir.join("skills"), load_skills_deduped),
    ];
    if let Some(home) = home {
        sites.push(Site::new(
            home.join(".claude").join("skills"),
            load_skills_deduped,
        ));
        sites.push(Site::new(
            home.join(".agents").join("skills"),
            load_skills_deduped,
        ));
    }
    sites.push(Site::with_payload(
        config_dir.to_path_buf(),
        load_claude_md,
        &[("AGENTS.md", "AGENTS.md")],
    ));
    sites.push(Site::new(
        config_dir.join("kilo.json"),
        load_kilo_instructions,
    ));
    sites
}

/// Read all installed config (global scope) into one surface list — the
/// direct entry point the catalog tests drive; `analyze` goes through
/// `adapter::harness::catalog_surfaces`. `seen` dedups skill definitions
/// across sites.
pub fn read_global_surfaces(
    config_dir: &Path,
    home: Option<&Path>,
    seen: &mut HashSet<PathBuf>,
) -> Vec<Surface> {
    crate::adapter::harness::load_sites(&global_sites(config_dir, home), &Scope::Global, home, seen)
}

/// The kilocode per-project config sites, as data: the `.kilo` pair, the
/// three skills sites, `AGENTS.md` with the `.kilorules` file, and the
/// project `kilo.json`'s `instructions[]`.
pub(crate) fn project_sites(root: &Path) -> Vec<Site> {
    let kilo = root.join(".kilo");
    vec![
        Site::new(kilo.join("command"), load_commands),
        Site::new(kilo.join("agent"), load_agents),
        Site::new(kilo.join("skills"), load_skills_deduped),
        Site::new(root.join(".claude").join("skills"), load_skills_deduped),
        Site::new(root.join(".agents").join("skills"), load_skills_deduped),
        Site::with_payload(
            root.to_path_buf(),
            load_claude_md,
            &[("AGENTS.md", "AGENTS.md"), (".kilorules", ".kilorules")],
        ),
        Site::new(root.join("kilo.json"), load_kilo_instructions),
    ]
}

/// Read one project's kilo config into surfaces — the direct entry point the
/// catalog tests drive (see `read_global_surfaces`). `seen` skips skill
/// definitions already catalogued from the global sites.
pub fn read_project_surfaces(
    root: &Path,
    project: &str,
    home: Option<&Path>,
    seen: &mut HashSet<PathBuf>,
) -> Vec<Surface> {
    crate::adapter::harness::load_sites(
        &project_sites(root),
        &Scope::Project(project.to_string()),
        home,
        seen,
    )
}

/// The files in `kilo.json`'s `instructions[]`, as always-on instruction
/// surfaces — the measured project pointed the array at a gitignored
/// `AGENTS.local.md` and the measured global config at an absolute path, and
/// a file only reachable through it is otherwise invisible to the catalog
/// while costing tokens on every session. Relative paths resolve against
/// `base_dir` (the config file's directory at the global scope, the project
/// root at the project scope); absolute paths pass through. A missing config,
/// a missing key, or a non-array yields nothing.
pub(crate) fn read_kilo_instructions(
    config_path: &Path,
    base_dir: &Path,
    scope: &Scope,
    home: Option<&Path>,
) -> Vec<Surface> {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(entries) = value.get("instructions").and_then(Value::as_array) else {
        return Vec::new();
    };
    let files: Vec<(PathBuf, String)> = entries
        .iter()
        .filter_map(|entry| entry.as_str())
        .filter_map(|entry| {
            let path = if Path::new(entry).is_absolute() {
                PathBuf::from(entry)
            } else {
                base_dir.join(entry)
            };
            let id = path.file_name()?.to_str()?.to_string();
            Some((path, id))
        })
        .collect();
    let refs: Vec<(&Path, &str)> = files
        .iter()
        .map(|(path, id)| (path.as_path(), id.as_str()))
        .collect();
    crate::adapter::config::read_claude_md_surfaces(&refs, scope, home)
}

/// The two files' stat folded into one per-task fingerprint: the latest mtime
/// (either file moves on activity) and the summed size.
fn task_fingerprint(conversation: &Path, ui: &Path) -> Option<(i64, i64)> {
    let conversation = file_fingerprint(conversation)?;
    let ui = file_fingerprint(ui)?;
    Some((conversation.0.max(ui.0), conversation.1 + ui.1))
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

/// Analyze kilocode task directories into the kilocode store. The file-to-
/// record mapping is `adapter::kilocode`; this shell reads each task's two
/// JSON files and ingests each task idempotently. See `docs/specs/harness.md`
/// for the v1 capability boundaries (no subagent runs, no prompts, no
/// permission denials, no surface catalog yet).
pub fn analyze(input: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
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
    let mut store = Store::open(db, OpenMode::ReadWrite).context("open store")?;
    // The kilocode version key lives apart from the shared analyzer version:
    // a kilocode-only extraction change must never force other harnesses
    // through a full re-analyze (`adapter::kilocode::KILOCODE_ANALYZER_VERSION`).
    let reuse_ingested =
        store.is_analyzer_current(KILOCODE_ANALYZER_META_KEY, KILOCODE_ANALYZER_VERSION)?;

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
        let records = parse_session(&conversation, &ui_messages);
        let spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);
        let usage = extract_usage_events(&records);
        // The root is the stated heuristic (`adapter::kilocode::estimate_root`):
        // kilocode stamps no cwd anywhere, so the common parent directory of
        // the leading tool paths is the best answer — empty when none appears.
        let root = estimate_root(&conversation).unwrap_or_default();
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
        // `raw_errors` owns the strings; `error_rows` borrows them.
        let raw_errors = extract_tool_errors(&conversation);
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
        // `work` owns the strings; the rows borrow them.
        let work = extract_work_events(&conversation);
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
        store.ingest(
            IngestBatch::new(&meta, &spans, &usage)
                .tool_errors(&error_rows)
                .work_events(&work_rows),
        )?;
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
    let surfaces = catalog_surfaces(Harness::Kilocode, &store.session_roots()?)?;
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    store.record_analyzer_version(KILOCODE_ANALYZER_META_KEY, KILOCODE_ANALYZER_VERSION)?;
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

/// The kilo config root for this run: the environment read that
/// `adapter::harness::kilocode_config_dir` deliberately does not do.
pub(crate) fn kilo_home() -> Result<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Harness::kilocode_config_dir(config_home.as_deref(), home_dir().map(Path::new))
        .context("set XDG_CONFIG_HOME or HOME")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::friction::ErrorCategory;
    use crate::core::surface::Scope;
    use serde_json::json;
    use std::collections::HashSet;

    fn unique_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn read_global_surfaces_walks_the_kilo_layout() {
        let config_dir = unique_dir("saniwalens-test-kilo-config");
        std::fs::remove_dir_all(&config_dir).ok();
        std::fs::create_dir_all(config_dir.join("command")).unwrap();
        std::fs::write(
            config_dir.join("command/review.md"),
            "---\ndescription: review\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(config_dir.join("agent")).unwrap();
        std::fs::write(
            config_dir.join("agent/scout.md"),
            "---\ndescription: scouts\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(config_dir.join("skills/reviewer")).unwrap();
        std::fs::write(
            config_dir.join("skills/reviewer/SKILL.md"),
            "---\ndescription: reviews\n---\nbody",
        )
        .unwrap();
        std::fs::write(config_dir.join("AGENTS.md"), "global instructions").unwrap();
        // kilo.json's instructions[] is kilo's own addition: an always-on
        // instruction file otherwise invisible to the catalog.
        std::fs::write(
            config_dir.join("kilo.json"),
            r#"{"instructions": ["RTK.md"]}"#,
        )
        .unwrap();
        std::fs::write(config_dir.join("RTK.md"), "tooling guidance").unwrap();

        let mut seen = HashSet::new();
        let surfaces = read_global_surfaces(&config_dir, None, &mut seen);
        std::fs::remove_dir_all(&config_dir).ok();

        let has = |kind: &str, id: &str| surfaces.iter().any(|s| s.kind == kind && s.id == id);
        assert!(surfaces.iter().all(|s| s.scope == Scope::Global));
        assert!(has("command", "review"));
        assert!(has("agent", "scout"));
        assert!(has("skill", "reviewer"));
        assert!(has("claude_md", "AGENTS.md"));
        assert!(has("claude_md", "RTK.md"));
    }

    #[test]
    fn read_project_surfaces_walks_the_kilo_layout() {
        let root = unique_dir("saniwalens-test-kilo-project");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join(".kilo/command")).unwrap();
        std::fs::write(
            root.join(".kilo/command/review.md"),
            "---\ndescription: review\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".kilo/agent")).unwrap();
        std::fs::write(
            root.join(".kilo/agent/scout.md"),
            "---\ndescription: scouts\n---\nbody",
        )
        .unwrap();
        std::fs::write(root.join("AGENTS.md"), "project instructions").unwrap();
        std::fs::write(root.join(".kilorules"), "project rules").unwrap();
        std::fs::write(
            root.join("kilo.json"),
            r#"{"instructions": ["AGENTS.local.md"]}"#,
        )
        .unwrap();
        std::fs::write(root.join("AGENTS.local.md"), "local instructions").unwrap();

        let mut seen = HashSet::new();
        let surfaces = read_project_surfaces(&root, "demo", None, &mut seen);
        std::fs::remove_dir_all(&root).ok();

        let has = |kind: &str, id: &str| surfaces.iter().any(|s| s.kind == kind && s.id == id);
        assert!(
            surfaces
                .iter()
                .all(|s| s.scope == Scope::Project("demo".to_string()))
        );
        assert!(has("command", "review"));
        assert!(has("agent", "scout"));
        assert!(has("claude_md", "AGENTS.md"));
        assert!(has("claude_md", ".kilorules"));
        assert!(has("claude_md", "AGENTS.local.md"));
    }

    fn conversation(messages: Vec<serde_json::Value>) -> String {
        serde_json::Value::Array(messages).to_string()
    }

    fn ui_file(rows: Vec<serde_json::Value>) -> String {
        serde_json::Value::Array(rows).to_string()
    }

    fn user_text(ts: i64, text: &str) -> serde_json::Value {
        json!({"role": "user", "ts": ts, "content": [{"type": "text", "text": text}]})
    }

    fn user_tool_result(
        ts: i64,
        tool_use_id: &str,
        is_error: bool,
        text: &str,
    ) -> serde_json::Value {
        json!({"role": "user", "ts": ts,
               "content": [{"type": "tool_result", "tool_use_id": tool_use_id,
                            "is_error": is_error, "content": text}]})
    }

    fn assistant_tool_use(
        ts: i64,
        id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> serde_json::Value {
        json!({"role": "assistant", "ts": ts,
               "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]})
    }

    fn assistant_text(ts: i64, text: &str) -> serde_json::Value {
        json!({"role": "assistant", "ts": ts, "content": [{"type": "text", "text": text}]})
    }

    fn usage_row(
        ts: i64,
        tokens_in: u64,
        tokens_out: u64,
        cache_reads: u64,
        cache_writes: u64,
        usage_missing: bool,
    ) -> serde_json::Value {
        json!({"ts": ts, "type": "say", "say": "api_req_started",
               "text": json!({"apiProtocol": "example", "tokensIn": tokens_in,
                              "tokensOut": tokens_out, "cacheWrites": cache_writes,
                              "cacheReads": cache_reads, "cost": 0.01,
                              "usageMissing": usage_missing,
                              "inferenceProvider": "example-provider"})
                   .to_string()})
    }

    fn other_row(ts: i64, kind: &str, say: &str) -> serde_json::Value {
        json!({"ts": ts, "type": kind, "say": say, "text": "synthetic display text"})
    }

    fn cost_of(records: &[Record]) -> (i64, u64, u64, String) {
        let cost = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Assistant { .. }))
            .unwrap_or_else(|| panic!("no assistant cost record in {records:?}"));
        match &cost.kind {
            RecordKind::Assistant {
                prompt_size,
                out_tokens,
                model,
            } => (cost.timestamp_ms, *prompt_size, *out_tokens, model.clone()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_user_message_with_text_yields_a_human_turn() {
        let conversation = conversation(vec![user_text(1_700_000_000_000, "fix the bug")]);
        let records = parse_session(&conversation, "[]");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp_ms, 1_700_000_000_000);
        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
    }

    #[test]
    fn a_tool_result_only_user_message_is_not_a_human_turn() {
        // The Anthropic-format mirror of a tool result rides under role=user;
        // only text material delimits spans.
        let conversation =
            conversation(vec![user_tool_result(1_700_000_000_000, "t1", false, "ok")]);
        let records = parse_session(&conversation, "[]");
        assert!(records.is_empty());
    }

    #[test]
    fn a_user_message_with_text_beside_a_tool_result_is_a_human_turn() {
        let conversation = conversation(vec![json!({"role": "user", "ts": 5, "content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": false, "content": "ok"},
            {"type": "text", "text": "and now also this"}
        ]})]);
        let records = parse_session(&conversation, "[]");
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
    }

    #[test]
    fn a_bare_string_user_content_yields_a_human_turn() {
        let conversation = conversation(vec![json!({"role": "user", "ts": 5, "content": "hello"})]);
        let records = parse_session(&conversation, "[]");
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
    }

    #[test]
    fn each_api_req_started_yields_one_assistant_record_with_the_joined_tokens() {
        // prompt_size = tokensIn + cacheReads + cacheWrites (the documented
        // assumption); out_tokens = tokensOut; the model stays "unknown".
        let conversation = conversation(vec![
            user_text(1000, "task"),
            assistant_text(2000, "working on it"),
        ]);
        let ui = ui_file(vec![usage_row(1500, 12_000, 400, 9_000, 800, false)]);
        let records = parse_session(&conversation, &ui);
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
        let (stamp, prompt_size, out_tokens, model) = cost_of(&records);
        assert_eq!(stamp, 2000);
        assert_eq!(prompt_size, 12_000 + 9_000 + 800);
        assert_eq!(out_tokens, 400);
        assert_eq!(model, "unknown");
    }

    #[test]
    fn the_cost_record_stamps_the_windows_last_assistant_message() {
        // One request window can hold several assistant messages (observed:
        // pairs seconds apart); the cost lands at the request's completion
        // point — the last one — not at the row or the first message.
        let conversation = conversation(vec![
            assistant_tool_use(
                2000,
                "t1",
                "read_file",
                json!({"path": "/tmp/example/a.rs"}),
            ),
            assistant_tool_use(
                4000,
                "t2",
                "execute_command",
                json!({"command": "cargo test"}),
            ),
        ]);
        let ui = ui_file(vec![usage_row(1000, 10_000, 300, 0, 0, false)]);
        let records = parse_session(&conversation, &ui);
        let (stamp, _, _, _) = cost_of(&records);
        assert_eq!(stamp, 4000);
    }

    #[test]
    fn a_request_without_an_assistant_message_stamps_the_row_itself_with_zero_tokens() {
        // A failed or cancelled request (usageMissing=true) still happened:
        // recorded with zeros at the row's own ts, never fabricated.
        let conversation = conversation(vec![user_text(1000, "task")]);
        let ui = ui_file(vec![usage_row(1500, 0, 0, 0, 0, true)]);
        let records = parse_session(&conversation, &ui);
        let (stamp, prompt_size, out_tokens, _) = cost_of(&records);
        assert_eq!(stamp, 1500);
        assert_eq!(prompt_size, 0);
        assert_eq!(out_tokens, 0);
    }

    #[test]
    fn assistant_messages_outside_any_window_carry_no_cost_record() {
        // Two request rows but one assistant message before the first row:
        // the orphan carries no cost record (no measured request row, never
        // fabricated), and the second window is empty — its row stamps itself.
        let conversation = conversation(vec![
            assistant_text(500, "orphan response"),
            assistant_text(2500, "claimed response"),
        ]);
        let ui = ui_file(vec![
            usage_row(1000, 10_000, 300, 0, 0, false),
            usage_row(3000, 11_000, 300, 0, 0, false),
        ]);
        let records = parse_session(&conversation, &ui);
        let costs: Vec<i64> = records
            .iter()
            .filter(|r| matches!(r.kind, RecordKind::Assistant { .. }))
            .map(|r| r.timestamp_ms)
            .collect();
        assert_eq!(costs, vec![2500, 3000]);
    }

    #[test]
    fn tool_use_blocks_become_tool_use_records() {
        let conversation = conversation(vec![assistant_tool_use(
            2000,
            "t1",
            "execute_command",
            json!({"command": "cargo test"}),
        )]);
        let records = parse_session(&conversation, "[]");
        assert_eq!(records.len(), 1);
        match &records[0].kind {
            RecordKind::ToolUse { tool } => assert_eq!(tool, "execute_command"),
            other => panic!("expected a tool-use record, got {other:?}"),
        }
    }

    #[test]
    fn tool_records_come_before_the_windows_cost_record() {
        // The tool record and the cost record can share the window's last ts;
        // the tool record comes first (the opencode adapter's guarantee).
        let conversation = conversation(vec![assistant_tool_use(
            2000,
            "t1",
            "apply_diff",
            json!({"path": "/tmp/example/a.rs"}),
        )]);
        let ui = ui_file(vec![usage_row(1000, 10_000, 300, 0, 0, false)]);
        let records = parse_session(&conversation, &ui);
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0].kind, RecordKind::ToolUse { .. }));
        assert!(matches!(records[1].kind, RecordKind::Assistant { .. }));
        assert_eq!(records[1].timestamp_ms, 2000);
    }

    #[test]
    fn records_sort_in_merged_ts_order() {
        // The two files interleave: each request row claims the assistant
        // messages that follow it, and every record lands in ts order.
        let conversation = conversation(vec![
            user_text(1000, "first"),
            assistant_text(3000, "first response"),
            user_text(4000, "second"),
            assistant_text(6000, "second response"),
        ]);
        let ui = ui_file(vec![
            usage_row(2000, 10_000, 300, 0, 0, false),
            usage_row(5000, 11_000, 300, 0, 0, false),
        ]);
        let records = parse_session(&conversation, &ui);
        let stamps: Vec<i64> = records.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(stamps, vec![1000, 3000, 4000, 6000]);
    }

    #[test]
    fn unknown_ui_kinds_are_ignored() {
        // Display furniture (checkpoint saves, assistant text, retry delays)
        // yields no records; only api_req_started is read.
        let conversation = conversation(vec![user_text(1000, "task")]);
        let ui = ui_file(vec![
            other_row(1100, "say", "checkpoint_saved"),
            other_row(1200, "say", "text"),
            other_row(1300, "say", "api_req_retry_delayed"),
            other_row(1400, "ask", "tool"),
        ]);
        let records = parse_session(&conversation, &ui);
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
    }

    #[test]
    fn unknown_content_block_types_are_ignored() {
        let conversation = conversation(vec![json!({"role": "assistant", "ts": 2000, "content": [
            {"type": "image", "source": {"type": "base64"}},
            {"type": "text", "text": "caption"}
        ]})]);
        let records = parse_session(&conversation, "[]");
        assert!(records.is_empty());
    }

    #[test]
    fn an_unparseable_ui_file_leaves_the_conversation_spine() {
        // Without a token source there is no measured request row, so no cost
        // record — but the conversation's own records still stand.
        let conversation = conversation(vec![
            user_text(1000, "task"),
            assistant_tool_use(
                2000,
                "t1",
                "read_file",
                json!({"path": "/tmp/example/a.rs"}),
            ),
        ]);
        let records = parse_session(&conversation, "not json");
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
        assert!(matches!(records[1].kind, RecordKind::ToolUse { .. }));
    }

    #[test]
    fn an_unparseable_conversation_file_yields_no_records() {
        let records = parse_session("not json", "[]");
        assert!(records.is_empty());
    }

    #[test]
    fn a_failed_tool_result_pairs_back_to_its_tool_use() {
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "execute_command",
                json!({"command": "cargo build --release"}),
            ),
            user_tool_result(2000, "t1", true, "command not found: cargo"),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        let error = &errors[0];
        assert_eq!(error.epoch_ms, 2000);
        assert_eq!(error.tool, "execute_command");
        assert_eq!(error.target, "cargo build --release");
        assert_eq!(error.category, ErrorCategory::CommandNotFound);
        assert_eq!(error.excerpt, "command not found: cargo");
    }

    #[test]
    fn an_error_excerpt_is_the_message_not_the_envelope() {
        // kilocode wraps tool failures in a JSON envelope; truncating the whole
        // blob mis-classified the failure — the envelope's own wording ("does
        // not exist") matched the path vocabulary and its fields matched
        // timeout. The excerpt is the readable message the tool carried.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "search_and_replace",
                json!({"path": "src/a.rs"}),
            ),
            user_tool_result(
                2000,
                "t1",
                true,
                r#"{"status":"error","message":"The tool execution failed","error":"Tool \"search_and_replace\" is not allowed in code mode."}"#,
            ),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].excerpt,
            "Tool \"search_and_replace\" is not allowed in code mode."
        );
        assert_eq!(errors[0].category, ErrorCategory::Other);
    }

    #[test]
    fn a_tool_result_text_blocks_unwrap_their_envelope() {
        // The measured shape: the failure rides in a text block as a JSON
        // envelope, followed by an environment_details block — joining the
        // block texts made the joined string non-JSON, so the envelope unwrap
        // never fired and the blob was stored whole. Each block unwraps.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "search_and_replace",
                json!({"path": "src/a.rs"}),
            ),
            json!({"role": "user", "ts": 2000, "content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                 "content": [
                    {"type": "text", "text": r#"{"status":"error","message":"The tool execution failed","error":"Tool \"search_and_replace\" is not allowed in code mode.","timeout":30000}"#},
                    {"type": "text", "text": "<environment_details>\n# VSCode Visible Files\nsrc/a.rs"}
                 ]}
            ]}),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        // The environment_details decoration is dropped: the editor's state
        // is not the failure's cause.
        assert_eq!(
            errors[0].excerpt,
            "Tool \"search_and_replace\" is not allowed in code mode."
        );
        assert_eq!(errors[0].category, ErrorCategory::Other);
    }

    #[test]
    fn the_environment_decoration_is_not_the_failure() {
        // kilo appends an environment_details block to every tool_result —
        // the editor's state, not the failure's cause. Its file names matched
        // the vocabulary ("timeout" in a test name) and mis-classified the
        // failure; the decoration is dropped from the message.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "search_and_replace",
                json!({"path": "src/a.rs"}),
            ),
            json!({"role": "user", "ts": 2000, "content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                 "content": [
                    {"type": "text", "text": r#"{"status":"error","message":"The tool execution failed","error":"Tool \"search_and_replace\" is not allowed in code mode."}"#},
                    {"type": "text", "text": "<environment_details>\n# VSCode Visible Files\ntimeout.test.ts"}
                 ]}
            ]}),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::Other);
        assert_eq!(
            errors[0].excerpt,
            "Tool \"search_and_replace\" is not allowed in code mode."
        );
    }

    #[test]
    fn the_failure_is_classified_on_the_message_not_the_envelope() {
        // The real envelope carries fields of its own — "timeout" among them —
        // and classifying on the envelope matched the wrong vocabulary. The
        // category comes from the same readable text the excerpt carries.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "search_and_replace",
                json!({"path": "src/a.rs"}),
            ),
            user_tool_result(
                2000,
                "t1",
                true,
                r#"{"status":"error","message":"The tool execution failed","error":"Tool \"search_and_replace\" is not allowed in code mode.","timeout":30000}"#,
            ),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::Other);
    }

    #[test]
    fn a_failure_without_a_matching_call_names_the_tool_unknown() {
        // A tool result whose call is not in the file (or whose id is unknown)
        // still counts: the tool is named `unknown` and the excerpt stands.
        let conversation = conversation(vec![user_tool_result(2000, "t9", true, "boom")]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "unknown");
        assert_eq!(errors[0].target, "");
        assert_eq!(errors[0].excerpt, "boom");
    }

    #[test]
    fn a_successful_tool_result_is_not_a_failure() {
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "read_file",
                json!({"path": "/tmp/example/a.rs"}),
            ),
            user_tool_result(2000, "t1", false, "file contents"),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert!(errors.is_empty());
    }

    #[test]
    fn an_array_tool_result_content_yields_its_text_excerpt() {
        // The common shape: the result content is an array of text blocks,
        // not a bare string.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "apply_diff",
                json!({"path": "/tmp/example/a.rs"}),
            ),
            json!({"role": "user", "ts": 2000, "content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                 "content": [{"type": "text", "text": "old string not found"}]}
            ]}),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "apply_diff");
        assert_eq!(errors[0].target, "/tmp/example/a.rs");
        assert_eq!(errors[0].excerpt, "old string not found");
    }

    #[test]
    fn an_execute_command_becomes_a_bash_work_event() {
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "execute_command",
            json!({"command": "cargo test --lib"}),
        )]);
        let events = extract_work_events(&conversation);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].epoch_ms, 1000);
        assert_eq!(events[0].kind, "bash_cmd");
        assert_eq!(events[0].id, "cargo");
        assert_eq!(events[0].path, None);
    }

    #[test]
    fn an_apply_diff_becomes_a_file_edit_work_event() {
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "apply_diff",
            json!({"path": "/tmp/example/src/lib.rs", "diff": "--- a\n+++ b"}),
        )]);
        let events = extract_work_events(&conversation);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "file_edit");
        assert_eq!(events[0].id, "lib.rs");
        assert_eq!(events[0].path.as_deref(), Some("/tmp/example/src/lib.rs"));
    }

    #[test]
    fn a_write_to_file_becomes_a_file_edit_work_event() {
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "write_to_file",
            json!({"path": "/tmp/example/src/main.rs", "content": "fn main() {}"}),
        )]);
        let events = extract_work_events(&conversation);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "file_edit");
        assert_eq!(events[0].id, "main.rs");
    }

    #[test]
    fn a_read_file_is_not_a_work_event() {
        // Reads change nothing, so they are not work — a hotspot list would
        // otherwise rank them beside the edits.
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "read_file",
            json!({"files": ["/tmp/example/a.rs"]}),
        )]);
        let events = extract_work_events(&conversation);
        assert!(events.is_empty());
    }

    #[test]
    fn an_edit_file_is_a_file_edit() {
        // The measured edit tool is `edit_file` (361 of 555 edit calls) and
        // its path key is `file_path` — `apply_diff` was only 101, so mapping
        // it alone under-counted the edits by four to one.
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "edit_file",
            json!({"file_path": "src/a.rs", "old_string": "x", "new_string": "y"}),
        )]);
        let events = extract_work_events(&conversation);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "file_edit");
        assert_eq!(events[0].id, "a.rs");
        assert_eq!(events[0].path.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn a_search_and_replace_is_a_file_edit() {
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "search_and_replace",
            json!({"path": "src/b.rs", "operations": []}),
        )]);
        let events = extract_work_events(&conversation);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "file_edit");
        assert_eq!(events[0].id, "b.rs");
    }

    #[test]
    fn an_edit_file_error_carries_its_target() {
        // `file_path` names the edit_file target; without it every edit_file
        // failure came out with an unknown target.
        let conversation = conversation(vec![
            assistant_tool_use(1000, "t1", "edit_file", json!({"file_path": "src/a.rs"})),
            user_tool_result(2000, "t1", true, "ENOENT: no such file"),
        ]);
        let errors = extract_tool_errors(&conversation);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "edit_file");
        assert_eq!(errors[0].target, "src/a.rs");
    }

    #[test]
    fn the_root_is_the_leading_paths_common_parent_dir() {
        // Tool paths in one directory share it; a later path outside the
        // established root does not drag the estimate down.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "read_file",
                json!({"path": "/tmp/example/proj/src/a.rs"}),
            ),
            assistant_tool_use(
                2000,
                "t2",
                "apply_diff",
                json!({"path": "/tmp/example/proj/README.md"}),
            ),
            assistant_tool_use(
                3000,
                "t3",
                "write_to_file",
                json!({"path": "/tmp/other/outside.md"}),
            ),
        ]);
        assert_eq!(
            estimate_root(&conversation).as_deref(),
            Some("/tmp/example/proj")
        );
    }

    #[test]
    fn the_first_execute_command_cwd_is_the_root() {
        // kilocode carries a `cwd` on execute_command inputs, and every task
        // that carries one names exactly one distinct value — the same
        // confidence as Claude Code's first-`cwd` root. Tool paths are
        // overwhelmingly relative (945 of 983 measured), so without this the
        // per-project cut is empty.
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "execute_command",
                json!({"command": "cargo test", "cwd": "/tmp/example/proj"}),
            ),
            assistant_tool_use(
                2000,
                "t2",
                "edit_file",
                json!({"file_path": "src/a.rs", "old_string": "x", "new_string": "y"}),
            ),
        ]);
        assert_eq!(
            estimate_root(&conversation).as_deref(),
            Some("/tmp/example/proj")
        );
    }

    #[test]
    fn the_cwd_wins_even_when_tool_paths_are_deeper() {
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "edit_file",
                json!({"file_path": "/tmp/example/proj/src/a.rs"}),
            ),
            assistant_tool_use(
                2000,
                "t2",
                "execute_command",
                json!({"command": "npm test", "cwd": "/tmp/example/proj"}),
            ),
        ]);
        assert_eq!(
            estimate_root(&conversation).as_deref(),
            Some("/tmp/example/proj")
        );
    }

    #[test]
    fn a_relative_cwd_is_skipped() {
        // A relative cwd carries no root signal; the leading-path rule (or
        // none) stands in.
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "execute_command",
            json!({"command": "cargo test", "cwd": "src"}),
        )]);
        assert_eq!(estimate_root(&conversation), None);
    }

    #[test]
    fn a_single_tool_path_estimates_its_parent_dir() {
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "read_file",
            json!({"path": "/tmp/example/proj/src/a.rs"}),
        )]);
        assert_eq!(
            estimate_root(&conversation).as_deref(),
            Some("/tmp/example/proj/src")
        );
    }

    #[test]
    fn files_arrays_contribute_paths() {
        let conversation = conversation(vec![
            assistant_tool_use(
                1000,
                "t1",
                "read_file",
                json!({"files": ["/tmp/example/proj/src/a.rs"]}),
            ),
            assistant_tool_use(
                2000,
                "t2",
                "write_to_file",
                json!({"path": "/tmp/example/proj/src/b.rs"}),
            ),
        ]);
        assert_eq!(
            estimate_root(&conversation).as_deref(),
            Some("/tmp/example/proj/src")
        );
    }

    #[test]
    fn relative_paths_are_skipped() {
        // A tool input naming a relative path carries no root signal.
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "write_to_file",
            json!({"path": "src/a.rs"}),
        )]);
        assert_eq!(estimate_root(&conversation), None);
    }

    #[test]
    fn a_task_with_no_absolute_paths_is_unrooted() {
        let conversation = conversation(vec![assistant_tool_use(
            1000,
            "t1",
            "execute_command",
            json!({"command": "cargo test"}),
        )]);
        assert_eq!(estimate_root(&conversation), None);
    }
}
