//! Parse opencode session data (SQLite `message`/`part` rows) into domain records.
//!
//! `parse_session` is pure over the rows' JSON — reading the database file is a
//! thin shell around it. See `docs/specs/session-format-opencode.md`. Like the
//! Claude Code adapter it deserializes defensively: only the needed fields,
//! unknown types ignored, missing optional ones tolerated.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::adapter::transcript::{ToolError, WorkEvent};
use crate::core::friction::classify_error;
use crate::core::path::basename;
use crate::core::span::{Record, RecordKind, Source};
use crate::core::surface::{LoadMode, Scope, Surface};

/// The model label when a message names none — honest, never a guess.
const UNKNOWN_MODEL: &str = "unknown";

/// The analysis semantics the opencode rows were built with. **Bump it
/// whenever opencode extraction changes what an unchanged database yields.**
/// It lives apart from the shared `ANALYZER_VERSION` (`store.rs`) on
/// purpose: an opencode-only change must never force every Claude Code user
/// through a full re-analyze. Stored under its own `meta` key.
pub const OPENCODE_ANALYZER_VERSION: &str = "2";

/// The `meta` key holding the opencode analysis semantics.
pub const OPENCODE_ANALYZER_META_KEY: &str = "opencode_analyzer_version";

/// One `message` row: the columns plus its JSON `data` blob.
pub struct MessageRow<'a> {
    pub id: &'a str,
    pub session_id: &'a str,
    pub time_created_ms: i64,
    pub parent_id: Option<&'a str>,
    pub data: &'a Value,
}

/// One `part` row: the columns plus its JSON `data` blob.
pub struct PartRow<'a> {
    pub id: &'a str,
    pub message_id: &'a str,
    pub session_id: &'a str,
    pub time_created_ms: i64,
    pub data: &'a Value,
}

/// Parse one session's rows into domain records, in transcript order.
///
/// Messages order by `time_created` (ties by id); within a message the
/// part-derived records come before that message's own cost record, so a span
/// opens at its invocation and includes the calling turn's cost. The cost
/// record is stamped with the turn's *end* (the latest part time, falling
/// back to the message time) — stamping it with the turn's start would invert
/// the stream whenever tools ran after the message was created, producing
/// negative durations. See `docs/specs/session-format-opencode.md`.
pub fn parse_session(messages: &[MessageRow], parts: &[PartRow]) -> Vec<Record> {
    let mut ordered: Vec<&MessageRow> = messages.iter().collect();
    ordered.sort_by_key(|m| (m.time_created_ms, m.id));

    let mut by_message: std::collections::HashMap<&str, Vec<&PartRow>> =
        std::collections::HashMap::new();
    for part in parts {
        by_message.entry(part.message_id).or_default().push(part);
    }
    for list in by_message.values_mut() {
        list.sort_by_key(|p| (part_time(p), p.id));
    }

    let mut records = Vec::new();
    for message in ordered {
        let role = message
            .data
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("");
        let message_parts: &[&PartRow] =
            by_message.get(message.id).map(Vec::as_slice).unwrap_or(&[]);
        match role {
            "user" => {
                let has_turn = message_parts.iter().any(|part| {
                    matches!(
                        part.data.get("type").and_then(Value::as_str),
                        Some("text") | Some("file")
                    )
                });
                if has_turn {
                    records.push(Record {
                        timestamp_ms: message.time_created_ms,
                        kind: RecordKind::HumanTurn,
                    });
                }
            }
            "assistant" => {
                for part in message_parts {
                    if let Some(kind) = part_record_kind(part, message) {
                        records.push(Record {
                            timestamp_ms: part_time(part),
                            kind,
                        });
                    }
                }
                let turn_end = message_parts
                    .iter()
                    .map(|part| part_time(part))
                    .max()
                    .unwrap_or(message.time_created_ms)
                    .max(message.time_created_ms);
                records.push(assistant_record(message, turn_end));
            }
            _ => {}
        }
    }
    records
}

/// One failed tool call per `part` whose `state.status` is `"error"`: its
/// friction category, a readable excerpt, the originating tool, and the call's
/// target from its input (`filePath` first — opencode spells it camelCase).
pub fn extract_tool_errors(parts: &[PartRow]) -> Vec<ToolError> {
    let mut errors = Vec::new();
    for part in parts {
        let state = part.data.get("state");
        if state.and_then(|s| s.get("status")).and_then(Value::as_str) != Some("error") {
            continue;
        }
        let text = state
            .and_then(|s| s.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let input = state.and_then(|s| s.get("input"));
        errors.push(ToolError {
            epoch_ms: part_time(part),
            category: classify_error(text),
            excerpt: truncate(text),
            tool: part_tool(part).unwrap_or("unknown").to_string(),
            target: tool_target(input),
        });
    }
    errors
}

/// One unit of work opencode performed: a Bash command's leading word or an
/// Edit/Write target's basename (with the full path alongside, as for Claude
/// Code work events).
pub fn extract_work_events(parts: &[PartRow]) -> Vec<WorkEvent> {
    let mut events = Vec::new();
    for part in parts {
        if part.data.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let input = part.data.get("state").and_then(|s| s.get("input"));
        match part_tool(part) {
            Some("bash") => {
                if let Some(cmd) = input
                    .and_then(|i| i.get("command"))
                    .and_then(Value::as_str)
                    .and_then(|c| c.split_whitespace().next())
                {
                    events.push(WorkEvent {
                        epoch_ms: part_time(part),
                        kind: "bash_cmd",
                        id: cmd.to_string(),
                        path: None,
                    });
                }
            }
            Some("edit") | Some("write") => {
                let path = tool_target(input);
                if !path.is_empty() {
                    events.push(WorkEvent {
                        epoch_ms: part_time(part),
                        kind: "file_edit",
                        id: basename(&path).to_string(),
                        path: Some(path),
                    });
                }
            }
            _ => {}
        }
    }
    events
}

/// When a part ran: its own `state.time.start`, else its row timestamp.
fn part_time(part: &PartRow<'_>) -> i64 {
    part.data
        .get("state")
        .and_then(|s| s.get("time"))
        .and_then(|t| t.get("start"))
        .and_then(Value::as_i64)
        .unwrap_or(part.time_created_ms)
}

/// A whole opencode database, read into owned rows. The CLI shell calls
/// `read_database` once and lends the rows to the pure mapping above, so no
/// SQL leaks past the adapter (`docs/specs/harness.md`).
pub struct Snapshot {
    pub sessions: Vec<SnapshotSession>,
    pub messages: Vec<SnapshotMessage>,
    pub parts: Vec<SnapshotPart>,
}

/// One `session` row: identity plus the fingerprint and project root.
pub struct SnapshotSession {
    pub id: String,
    pub slug: String,
    pub directory: String,
    pub time_created_ms: i64,
    pub time_updated_ms: i64,
}

/// One `message` row with its parsed JSON blob.
pub struct SnapshotMessage {
    pub id: String,
    pub session_id: String,
    pub time_created_ms: i64,
    pub data: Value,
}

/// One `part` row with its parsed JSON blob.
pub struct SnapshotPart {
    pub id: String,
    pub message_id: String,
    pub session_id: String,
    pub time_created_ms: i64,
    pub data: Value,
}

/// Read an opencode database file into a snapshot. Opened read-only: analysis
/// must never mutate the harness's own data. Rows whose `data` blob is not
/// readable JSON are skipped, never fatal — the database stays queryable even
/// when one row comes from a newer opencode.
pub fn read_database(path: &Path) -> Result<Snapshot> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open opencode database {}", path.display()))?;

    let mut sessions = Vec::new();
    let mut statement = conn.prepare(
        "SELECT id, slug, directory, time_created, time_updated
         FROM session ORDER BY time_created",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        sessions.push(SnapshotSession {
            id: row.get(0)?,
            slug: row.get(1)?,
            directory: row.get(2)?,
            time_created_ms: row.get(3)?,
            time_updated_ms: row.get(4)?,
        });
    }

    // Copy the text out of SQLite before parsing it: the parsed `Value`s are
    // owned by the snapshot, not borrowed from a row that is already gone.
    let texts: Vec<(String, String, i64, String)> = conn
        .prepare("SELECT id, session_id, time_created, data FROM message ORDER BY time_created")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut messages = Vec::new();
    for (id, session_id, time_created_ms, data) in texts {
        let Ok(data) = serde_json::from_str(&data) else {
            continue;
        };
        messages.push(SnapshotMessage {
            id,
            session_id,
            time_created_ms,
            data,
        });
    }

    let texts: Vec<(String, String, String, i64, String)> = conn
        .prepare(
            "SELECT id, message_id, session_id, time_created, data
             FROM part ORDER BY time_created",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut parts = Vec::new();
    for (id, message_id, session_id, time_created_ms, data) in texts {
        let Ok(data) = serde_json::from_str(&data) else {
            continue;
        };
        parts.push(SnapshotPart {
            id,
            message_id,
            session_id,
            time_created_ms,
            data,
        });
    }

    Ok(Snapshot {
        sessions,
        messages,
        parts,
    })
}

/// Build a `command` surface from one custom-command file. Commands run on
/// demand; only the frontmatter `description` is startup-listed, so the load
/// mode is `StartupDescription` — the same assumption as a skill body (see
/// `docs/specs/config-format-opencode.md`).
pub fn command_surface(id: &str, config_path: &str, content: &str, scope: &Scope) -> Surface {
    Surface {
        kind: "command".to_string(),
        id: id.to_string(),
        scope: scope.clone(),
        config_path: config_path.to_string(),
        static_tokens: Some(crate::adapter::config::approx_tokens(content)),
        startup_tokens: Some(description_tokens(content)),
        load_mode: LoadMode::StartupDescription,
    }
}

/// Build an `agent` surface from one custom-agent file. Like commands, only
/// the description loads at startup.
pub fn agent_surface(id: &str, config_path: &str, content: &str, scope: &Scope) -> Surface {
    Surface {
        kind: "agent".to_string(),
        id: id.to_string(),
        scope: scope.clone(),
        config_path: config_path.to_string(),
        static_tokens: Some(crate::adapter::config::approx_tokens(content)),
        startup_tokens: Some(description_tokens(content)),
        load_mode: LoadMode::StartupDescription,
    }
}

/// The startup weight of a description-listed surface: its frontmatter
/// `description`, or zero when it declares none.
fn description_tokens(content: &str) -> u64 {
    frontmatter_description(content).map_or(0, |d| crate::adapter::config::approx_tokens(&d))
}

/// The YAML frontmatter block of a markdown file, without its `---` fences.
/// The block ends at the first line that is *only* dashes.
fn frontmatter(content: &str) -> Option<&str> {
    let opened = content.trim_start().strip_prefix("---")?;
    let body = &opened[opened.find('\n')? + 1..];
    let mut end = 0;
    for line in body.split_inclusive('\n') {
        if line.trim() == "---" {
            return Some(&body[..end]);
        }
        end += line.len();
    }
    None
}

/// The frontmatter `description`, folded with its indented continuation
/// lines. Parsed defensively and opencode-side: a plain or quoted scalar, or
/// a block scalar; a missing key yields `None`.
fn frontmatter_description(content: &str) -> Option<String> {
    let mut lines = frontmatter(content)?.lines();
    let first = loop {
        match lines.next()?.strip_prefix("description:") {
            Some(value) => break value.trim(),
            None => continue,
        }
    };
    let block = first.starts_with('>') || first.starts_with('|');
    let mut folded: Vec<&str> = Vec::new();
    if !(first.is_empty() || block) {
        folded.push(first);
    }
    folded.extend(
        lines
            .take_while(|line| line.trim().is_empty() || line.starts_with([' ', '\t']))
            .map(|line| line.trim())
            .filter(|line| !line.is_empty()),
    );
    if folded.is_empty() {
        return None;
    }
    let value = folded.join(" ");
    Some(unquote(&value).to_string())
}

/// Strip one layer of matching surrounding quotes, if present.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Read every `*.md` command file in a directory into surfaces, keyed by file
/// stem. A missing directory yields nothing.
pub fn read_command_surfaces(dir: &Path, scope: &Scope) -> Vec<Surface> {
    read_markdown_surfaces(dir, scope, command_surface)
}

/// Read every `*.md` agent file in a directory into surfaces, keyed by file
/// stem. A missing directory yields nothing.
pub fn read_agent_surfaces(dir: &Path, scope: &Scope) -> Vec<Surface> {
    read_markdown_surfaces(dir, scope, agent_surface)
}

fn read_markdown_surfaces(
    dir: &Path,
    scope: &Scope,
    build: fn(&str, &str, &str, &Scope) -> Surface,
) -> Vec<Surface> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                let id = path.file_stem()?.to_str()?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                Some(build(&id, &path.display().to_string(), &content, scope))
            } else {
                None
            }
        })
        .collect()
}

/// Read the global opencode config into surfaces: `command/` and `agent/`
/// under the config dir. (No global `AGENTS.md` — whether opencode loads one
/// is unverified; see `docs/specs/config-format-opencode.md`.)
pub fn read_global_surfaces(config_dir: &Path) -> Vec<Surface> {
    let scope = Scope::Global;
    let mut surfaces = read_command_surfaces(&config_dir.join("command"), &scope);
    surfaces.extend(read_agent_surfaces(&config_dir.join("agent"), &scope));
    surfaces
}

/// Read one project's opencode config into surfaces: `.opencode/command/`,
/// `.opencode/agent/`, and `AGENTS.md` with its `@import`s. `home` resolves
/// a `~/` import.
pub fn read_project_surfaces(root: &Path, project: &str, home: Option<&Path>) -> Vec<Surface> {
    let scope = Scope::Project(project.to_string());
    let opencode = root.join(".opencode");
    let mut surfaces = read_command_surfaces(&opencode.join("command"), &scope);
    surfaces.extend(read_agent_surfaces(&opencode.join("agent"), &scope));
    let agents_md = root.join("AGENTS.md");
    surfaces.extend(crate::adapter::config::read_claude_md_surfaces(
        &[(&agents_md, "AGENTS.md")],
        &scope,
        home,
    ));
    surfaces
}

/// A part's tool name, when it is a `tool` part.
fn part_tool<'a>(part: &PartRow<'a>) -> Option<&'a str> {
    if part.data.get("type").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    part.data.get("tool").and_then(Value::as_str)
}

/// Classify one `tool` part into a domain record, or `None` for the kinds
/// that are not usage: `skill_use` (context loading, not invocation) and
/// `subtask` (the child-side mirror of a `task` spawn).
fn part_record_kind(part: &PartRow<'_>, message: &MessageRow<'_>) -> Option<RecordKind> {
    match part_tool(part)? {
        "skill" => {
            let skill = part
                .data
                .get("state")?
                .get("input")?
                .get("name")?
                .as_str()?
                .to_string();
            Some(RecordKind::SkillInvocation {
                skill,
                source: Source::Tool,
            })
        }
        "task" => {
            let input = part.data.get("state").and_then(|s| s.get("input"));
            let agent = input
                .and_then(|i| i.get("subagent_type"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            Some(RecordKind::AgentSpawn {
                agent,
                tool_use_id: part
                    .data
                    .get("callID")
                    .and_then(Value::as_str)
                    .map(String::from),
                prompt_id: message.parent_id.map(String::from),
            })
        }
        "skill_use" => None,
        tool => Some(RecordKind::ToolUse {
            tool: tool.to_string(),
        }),
    }
}

/// One assistant message's cost row. Message-level tokens are authoritative
/// (at most one `step-finish` part echoes them), so parts are never summed.
/// `prompt_size` mirrors the Claude Code sum — see
/// `docs/specs/session-format-opencode.md`. Stamped with the turn's end (see
/// `parse_session`), not the message creation time.
fn assistant_record(message: &MessageRow<'_>, turn_end_ms: i64) -> Record {
    let tokens = message.data.get("tokens");
    let number = |key: &str| {
        tokens
            .and_then(|t| t.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cache = tokens.and_then(|t| t.get("cache"));
    let cached = |key: &str| {
        cache
            .and_then(|c| c.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Record {
        timestamp_ms: turn_end_ms,
        kind: RecordKind::Assistant {
            prompt_size: number("input") + cached("read") + cached("write"),
            out_tokens: number("output"),
            model: message
                .data
                .get("modelID")
                .and_then(Value::as_str)
                .unwrap_or(UNKNOWN_MODEL)
                .to_string(),
        },
    }
}

/// The subject of a tool call: the first present identifying input field.
/// opencode spells the file key `filePath`.
fn tool_target(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    for key in ["filePath", "file_path", "path", "command", "pattern", "url"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return truncate(value);
        }
    }
    String::new()
}

/// Whitespace-collapse and truncate by Unicode scalar (never mid-character).
fn truncate(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::friction::ErrorCategory;
    use crate::core::span::Source;
    use crate::core::surface::{LoadMode, Scope};
    use serde_json::json;

    #[test]
    fn a_user_message_with_text_is_a_human_turn() {
        let data = json!({"role": "user"});
        let text = json!({"type": "text", "text": "do the thing"});
        let messages = [MessageRow {
            id: "msg_1",
            session_id: "ses_1",
            time_created_ms: 1_000,
            parent_id: None,
            data: &data,
        }];
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_1",
            session_id: "ses_1",
            time_created_ms: 1_000,
            data: &text,
        }];

        let records = parse_session(&messages, &parts);

        assert!(matches!(records[0].kind, RecordKind::HumanTurn));
    }

    #[test]
    fn a_user_message_with_only_compaction_is_not_a_human_turn() {
        let data = json!({"role": "user"});
        let compaction = json!({"type": "compaction", "auto": true});
        let messages = [MessageRow {
            id: "msg_1",
            session_id: "ses_1",
            time_created_ms: 1_000,
            parent_id: None,
            data: &data,
        }];
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_1",
            session_id: "ses_1",
            time_created_ms: 1_000,
            data: &compaction,
        }];

        assert!(parse_session(&messages, &parts).is_empty());
    }

    #[test]
    fn an_assistant_message_carries_its_own_cost() {
        let data = json!({
            "role": "assistant",
            "modelID": "preview/example-model",
            "tokens": {"input": 100, "output": 40, "cache": {"read": 900, "write": 10}},
        });
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: Some("msg_1"),
            data: &data,
        }];

        let records = parse_session(&messages, &[]);

        assert_eq!(records.len(), 1);
        let RecordKind::Assistant {
            prompt_size,
            out_tokens,
            model,
        } = &records[0].kind
        else {
            panic!("expected an assistant record, got {:?}", records[0].kind);
        };
        // The opencode prompt-size mirror: input + cache.read + cache.write.
        assert_eq!(*prompt_size, 1010);
        assert_eq!(*out_tokens, 40);
        assert_eq!(model, "preview/example-model");
    }

    #[test]
    fn an_assistant_message_without_a_model_is_unknown() {
        let data = json!({"role": "assistant", "tokens": {"input": 5, "output": 1}});
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: None,
            data: &data,
        }];

        let records = parse_session(&messages, &[]);

        let RecordKind::Assistant { model, .. } = &records[0].kind else {
            panic!("expected an assistant record, got {:?}", records[0].kind);
        };
        assert_eq!(model, "unknown");
    }

    #[test]
    fn a_skill_part_invokes_before_the_turn_cost() {
        let data = json!({"role": "assistant", "tokens": {"input": 5, "output": 1}});
        let skill = json!({
            "type": "tool", "tool": "skill", "callID": "call_1",
            "state": {"status": "completed", "input": {"name": "git-commit"}},
        });
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: None,
            data: &data,
        }];
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 1_900,
            data: &skill,
        }];

        let records = parse_session(&messages, &parts);

        assert_eq!(records.len(), 2);
        let RecordKind::SkillInvocation { skill, source } = &records[0].kind else {
            panic!(
                "expected a skill invocation first, got {:?}",
                records[0].kind
            );
        };
        assert_eq!(skill, "git-commit");
        assert_eq!(*source, Source::Tool);
        assert!(matches!(records[1].kind, RecordKind::Assistant { .. }));
    }

    #[test]
    fn a_task_part_spawns_with_its_call_and_parent_keys() {
        let data = json!({"role": "assistant", "tokens": {"input": 5, "output": 1}});
        let task = json!({
            "type": "tool", "tool": "task", "callID": "call_9",
            "state": {"status": "completed",
                      "input": {"subagent_type": "explore", "prompt": "look around"}},
        });
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: Some("msg_1"),
            data: &data,
        }];
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 1_900,
            data: &task,
        }];

        let records = parse_session(&messages, &parts);

        let RecordKind::AgentSpawn {
            agent,
            tool_use_id,
            prompt_id,
        } = &records[0].kind
        else {
            panic!("expected an agent spawn first, got {:?}", records[0].kind);
        };
        assert_eq!(agent, "explore");
        assert_eq!(tool_use_id.as_deref(), Some("call_9"));
        assert_eq!(prompt_id.as_deref(), Some("msg_1"));
    }

    #[test]
    fn subtask_and_skill_use_mirrors_emit_nothing() {
        let data = json!({"role": "assistant", "tokens": {"input": 5, "output": 1}});
        let subtask = json!({"type": "subtask", "agent": "build", "command": "review"});
        let skill_use = json!({
            "type": "tool", "tool": "skill_use", "callID": "call_2",
            "state": {"status": "completed", "input": {"skill_names": ["x"]}},
        });
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: None,
            data: &data,
        }];
        let parts = [
            PartRow {
                id: "prt_1",
                message_id: "msg_2",
                session_id: "ses_1",
                time_created_ms: 1_900,
                data: &subtask,
            },
            PartRow {
                id: "prt_2",
                message_id: "msg_2",
                session_id: "ses_1",
                time_created_ms: 1_910,
                data: &skill_use,
            },
        ];

        let records = parse_session(&messages, &parts);

        // Only the turn's own cost row; the mirrors are not invocations.
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].kind, RecordKind::Assistant { .. }));
    }

    #[test]
    fn other_tools_are_generic_tool_use() {
        let data = json!({"role": "assistant", "tokens": {"input": 5, "output": 1}});
        let read = json!({
            "type": "tool", "tool": "read", "callID": "call_3",
            "state": {"status": "completed", "input": {"filePath": "/tmp/example/a.md"}},
        });
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: None,
            data: &data,
        }];
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 1_900,
            data: &read,
        }];

        let records = parse_session(&messages, &parts);

        let RecordKind::ToolUse { tool } = &records[0].kind else {
            panic!("expected tool use first, got {:?}", records[0].kind);
        };
        assert_eq!(tool, "read");
    }

    #[test]
    fn failed_tools_keep_their_tool_and_camel_case_target() {
        let read = json!({
            "type": "tool", "tool": "read", "callID": "call_4",
            "state": {"status": "error", "error": "File does not exist: /tmp/example/foo.md",
                      "input": {"filePath": "/tmp/example/foo.md"}},
        });
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 1_900,
            data: &read,
        }];

        let errors = extract_tool_errors(&parts);

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::PathNotFound);
        assert_eq!(errors[0].tool, "read");
        assert_eq!(errors[0].target, "/tmp/example/foo.md");
        assert!(errors[0].excerpt.contains("/tmp/example/foo.md"));
    }

    #[test]
    fn an_unclassified_phrasing_is_kept_not_dropped() {
        // Classification reuses the shared `core::friction` vocabulary; a
        // phrasing it does not cover still yields its tool, target, and
        // excerpt under `Other` rather than disappearing.
        let read = json!({
            "type": "tool", "tool": "read", "callID": "call_7",
            "state": {"status": "error", "error": "widgets could not be evaluated",
                      "input": {"filePath": "/tmp/example/img.png"}},
        });
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 1_900,
            data: &read,
        }];

        let errors = extract_tool_errors(&parts);

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::Other);
        assert_eq!(errors[0].tool, "read");
        assert_eq!(errors[0].target, "/tmp/example/img.png");
    }

    #[test]
    fn work_events_cover_bash_and_camel_case_edits() {
        let bash = json!({
            "type": "tool", "tool": "bash", "callID": "call_5",
            "state": {"status": "completed", "input": {"command": "cargo test --all"}},
        });
        let edit = json!({
            "type": "tool", "tool": "edit", "callID": "call_6",
            "state": {"status": "completed", "input": {"filePath": "/tmp/example/cli.rs"}},
        });
        let parts = [
            PartRow {
                id: "prt_1",
                message_id: "msg_2",
                session_id: "ses_1",
                time_created_ms: 1_900,
                data: &bash,
            },
            PartRow {
                id: "prt_2",
                message_id: "msg_2",
                session_id: "ses_1",
                time_created_ms: 1_910,
                data: &edit,
            },
        ];

        let events = extract_work_events(&parts);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "bash_cmd");
        assert_eq!(events[0].id, "cargo");
        assert_eq!(events[1].kind, "file_edit");
        assert_eq!(events[1].id, "cli.rs");
        assert_eq!(events[1].path.as_deref(), Some("/tmp/example/cli.rs"));
    }

    /// A scratch database path unique to the calling test.
    fn scratch_db(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cclens-test-opencode-{name}-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&path).ok();
        path
    }

    #[test]
    fn reads_a_database_into_owned_rows() {
        let path = scratch_db("read");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session
               (id TEXT PRIMARY KEY, slug TEXT, directory TEXT,
                time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message
               (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part
               (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_1', 'example', '/tmp/example', 1000, 2000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES
             ('msg_1', 'ses_1', 1000, '{\"role\":\"user\"}'),
             ('msg_2', 'ses_1', 2000, '{\"role\":\"assistant\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES
             ('prt_1', 'msg_1', 'ses_1', 1000, '{\"type\":\"text\"}')",
            [],
        )
        .unwrap();
        drop(conn);

        let snapshot = read_database(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].directory, "/tmp/example");
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.parts.len(), 1);
    }

    #[test]
    fn unreadable_blobs_are_skipped_not_fatal() {
        let path = scratch_db("blobs");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session
               (id TEXT PRIMARY KEY, slug TEXT, directory TEXT,
                time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message
               (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part
               (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_1', 'example', '/tmp/example', 1000, 2000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_1', 'ses_1', 1000, 'not json')",
            [],
        )
        .unwrap();
        drop(conn);

        let snapshot = read_database(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(snapshot.sessions.len(), 1);
        assert!(snapshot.messages.is_empty());
    }

    #[test]
    fn a_missing_database_is_an_error() {
        let path = scratch_db("missing");
        assert!(read_database(&path).is_err());
    }

    #[test]
    fn the_cost_record_closes_the_turn_never_opens_it() {
        // Tools run after their message is created; stamping the cost row
        // with the creation time would invert the stream and produce
        // negative span durations.
        let data = json!({"role": "assistant", "tokens": {"input": 5, "output": 1}});
        let tool = json!({
            "type": "tool", "tool": "read", "callID": "call_8",
            "state": {"status": "completed",
                      "input": {"filePath": "/tmp/example/a.md"},
                      "time": {"start": 5_000, "end": 6_000}},
        });
        let messages = [MessageRow {
            id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            parent_id: None,
            data: &data,
        }];
        let parts = [PartRow {
            id: "prt_1",
            message_id: "msg_2",
            session_id: "ses_1",
            time_created_ms: 2_000,
            data: &tool,
        }];

        let records = parse_session(&messages, &parts);

        assert_eq!(records.len(), 2);
        assert!(records[0].timestamp_ms <= records[1].timestamp_ms);
        assert_eq!(records[1].timestamp_ms, 5_000);
        assert!(matches!(records[1].kind, RecordKind::Assistant { .. }));
    }

    #[test]
    fn a_command_pays_only_its_description_at_startup() {
        let content = "---\ndescription: review the branch diff\n---\n$ARGUMENTS";
        let surface = command_surface("review", "/cfg/command/review.md", content, &Scope::Global);

        assert_eq!(surface.kind, "command");
        assert_eq!(surface.id, "review");
        assert_eq!(surface.load_mode, LoadMode::StartupDescription);
        assert_eq!(
            surface.static_tokens,
            Some(crate::adapter::config::approx_tokens(content))
        );
        assert!(surface.startup_tokens.unwrap() < surface.static_tokens.unwrap());
    }

    #[test]
    fn a_description_less_command_costs_nothing_at_startup() {
        let surface = command_surface("bare", "/cfg/command/bare.md", "# body", &Scope::Global);
        assert_eq!(surface.startup_tokens, Some(0));
    }

    #[test]
    fn an_agent_pays_only_its_description_at_startup() {
        let content = "---\ndescription: explores the codebase\n---\nlong body";
        let surface = agent_surface("explore", "/cfg/agent/explore.md", content, &Scope::Global);

        assert_eq!(surface.kind, "agent");
        assert_eq!(surface.load_mode, LoadMode::StartupDescription);
        assert_eq!(surface.startup_tokens, Some(6));
    }

    #[test]
    fn a_missing_commands_dir_yields_nothing() {
        let surfaces = read_command_surfaces(Path::new("/tmp/example/no-such-dir"), &Scope::Global);
        assert!(surfaces.is_empty());
    }

    #[test]
    fn read_project_surfaces_walks_the_opencode_layout() {
        let root = std::env::temp_dir().join(format!(
            "cclens-test-opencode-project-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join(".opencode/command")).unwrap();
        std::fs::write(
            root.join(".opencode/command/review.md"),
            "---\ndescription: review\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".opencode/agent")).unwrap();
        std::fs::write(
            root.join(".opencode/agent/scout.md"),
            "---\ndescription: scouts\n---\nbody",
        )
        .unwrap();
        std::fs::write(root.join("AGENTS.md"), "project instructions").unwrap();

        let surfaces = read_project_surfaces(&root, "demo", None);
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
    }
}

/// One rendered slice of a `opencode run --format json` output line — what the
/// terminal shows while a headless optimize session runs. The upstream event
/// shapes are unversioned and vary across opencode releases, so the parse is
/// lenient: a line that carries no recognizable text yields `Silent` and the
/// raw line goes to the log file instead (`docs/specs/harness.md`).
#[derive(Debug, PartialEq)]
pub enum RunEvent {
    /// Agent-visible text: the narration lines and the final plan.
    Text(String),
    /// A compact one-line progress marker — a tool name (`read`, `shell`) or
    /// a part type (`step-start`) for events with no agent text.
    Progress(String),
    /// A line with nothing to show (not JSON, no type anywhere).
    Silent,
}

/// Render one `--format json` line. Text parts surface as `Text`; every other
/// typed part surfaces as a one-line `Progress` marker — so a headless run
/// streams the plan and never sits silent while the advisor works. The part
/// usually sits **nested** under `part` inside an event envelope whose own
/// `type` names the event (`tool_use`, `step_start`), so the nested part is
/// what gets rendered, with the top level as the fallback.
pub fn render_run_line(line: &str) -> RunEvent {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return RunEvent::Silent;
    };
    render_run_value(&v)
}

fn render_run_value(v: &Value) -> RunEvent {
    match v {
        Value::Array(items) => items
            .iter()
            .find_map(|item| match render_run_value(item) {
                RunEvent::Silent => None,
                event => Some(event),
            })
            .unwrap_or(RunEvent::Silent),
        Value::Object(_) => {
            let candidates = [
                v.get("part"),
                v.get("parts").and_then(|p| p.get(0)),
                Some(v),
            ];
            for candidate in candidates.into_iter().flatten() {
                let Some(kind) = candidate.get("type").and_then(Value::as_str) else {
                    continue;
                };
                return match kind {
                    "text" => match candidate.get("text").and_then(Value::as_str) {
                        Some(text) if !text.trim().is_empty() => RunEvent::Text(text.to_string()),
                        _ => RunEvent::Silent,
                    },
                    "tool" => match candidate
                        .get("tool")
                        .or_else(|| candidate.get("toolName"))
                        .or_else(|| candidate.get("name"))
                        .and_then(Value::as_str)
                    {
                        Some(tool) if !tool.is_empty() => RunEvent::Progress(tool.to_string()),
                        _ => RunEvent::Progress("tool".to_string()),
                    },
                    other => RunEvent::Progress(other.to_string()),
                };
            }
            RunEvent::Silent
        }
        _ => RunEvent::Silent,
    }
}

#[cfg(test)]
mod run_event_tests {
    use super::*;

    #[test]
    fn a_text_part_renders_its_text() {
        let line = r#"{"type":"text","text":"Plan: batch wiki edits"}"#;
        assert_eq!(
            render_run_line(line),
            RunEvent::Text("Plan: batch wiki edits".to_string())
        );
    }

    #[test]
    fn a_nested_text_part_renders_too() {
        let line = r#"{"part":{"type":"text","text":"narration"}}"#;
        assert_eq!(
            render_run_line(line),
            RunEvent::Text("narration".to_string())
        );
    }

    #[test]
    fn an_event_envelope_renders_the_nested_part() {
        // The real `--format json` shape: an envelope naming the event kind
        // (snake_case) wrapping the part (hyphenated types) under `part`.
        let tool = r#"{"type":"tool_use","timestamp":1792904532992,"sessionID":"ses_demo","part":{"partID":"prt_demo","type":"tool","tool":"read","state":{"status":"completed"}}}"#;
        assert_eq!(
            render_run_line(tool),
            RunEvent::Progress("read".to_string())
        );
        let text = r#"{"type":"text","timestamp":1792904882135,"sessionID":"ses_demo","part":{"id":"prt_demo","type":"text","text":"The plan covers three projects"}}"#;
        assert_eq!(
            render_run_line(text),
            RunEvent::Text("The plan covers three projects".to_string())
        );
    }

    #[test]
    fn an_unrecognized_envelope_renders_the_nested_type() {
        // A typed part with no agent text still marks progress — a headless
        // run never sits silent while the advisor works.
        let step =
            r#"{"type":"step_start","timestamp":1,"part":{"type":"step-start","snapshot":"abc"}}"#;
        assert_eq!(
            render_run_line(step),
            RunEvent::Progress("step-start".to_string())
        );
        let bare = r#"{"type":"tool","state":{"status":"pending"}}"#;
        assert_eq!(
            render_run_line(bare),
            RunEvent::Progress("tool".to_string())
        );
    }

    #[test]
    fn a_tool_part_renders_a_progress_marker() {
        let line = r#"{"type":"tool","tool":"bash","state":{"status":"running"}}"#;
        assert_eq!(
            render_run_line(line),
            RunEvent::Progress("bash".to_string())
        );
    }

    #[test]
    fn untyped_and_unparseable_lines_stay_silent() {
        for line in [
            r#"{"timestamp":1,"sessionID":"ses_demo"}"#,
            "not json at all",
            "",
        ] {
            assert_eq!(render_run_line(line), RunEvent::Silent, "{line}");
        }
    }

    #[test]
    fn an_empty_text_part_stays_silent() {
        assert_eq!(
            render_run_line(r#"{"type":"text","text":"  "}"#),
            RunEvent::Silent
        );
    }
}
