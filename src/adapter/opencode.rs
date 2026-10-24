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

/// The model label when a message names none — honest, never a guess.
const UNKNOWN_MODEL: &str = "unknown";

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
/// opens at its invocation and includes the calling turn's cost. See
/// `docs/specs/session-format-opencode.md`.
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
                records.push(assistant_record(message));
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
/// `docs/specs/session-format-opencode.md`.
fn assistant_record(message: &MessageRow<'_>) -> Record {
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
        timestamp_ms: message.time_created_ms,
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
            "state": {"status": "error", "error": "Cannot read binary file: /tmp/example/img.png",
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
}
