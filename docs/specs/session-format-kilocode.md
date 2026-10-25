# Session Format Specification — kilocode

This spec catalogues what the **adapter layer** knows about the **kilocode
session** format — the usage half for the kilocode harness. Its siblings are
`session-format.md` (Claude Code) and `session-format-opencode.md`; all three
share the same downstream contract (the internal domain model,
`core::span::Record`) but describe different upstream shapes. Only the
adapter may use the field names below
(`.claude/rules/format-isolation.md`). The kilocode store is **separate**
from the other stores (one DB per harness), so nothing here constrains the
other mappings, and vice versa. The shell contract around this mapping — the
harness's default `--db` name, per-task ingest keys, and extraction version
key — is the `harness.md` addendum that lands with the CLI wiring, as
opencode's did.

> Observed against a real `<VSCode globalStorage>/kilocode.kilo-code/tasks/`
> directory (92 task directories; the **shapes** below come from six of
> them). Counts drift as tasks accumulate; the **shapes** are what matter.
> Every value in an example position is a synthetic stand-in — no real ids,
> paths, or prompt text.

## Where sessions live

```text
<globalStorage>/kilocode.kilo-code/tasks/<task-uuid>/
├── api_conversation_history.json   # the conversation (the record spine)
├── ui_messages.json                # the display log (the token source)
├── task_metadata.json              # files_in_context[] only (unread in v1)
└── checkpoints/                    # editor checkpoints (unread)
```

- `<globalStorage>` is the editor's per-user extension storage
  (`~/Library/Application Support/Code/User/globalStorage` on macOS,
  `~/.config/Code/User/globalStorage` on Linux). The path varies with the
  editor install (Code / Cursor / VSCodium), so the adapter resolves the
  tasks directory from the harness's input root — `--projects`, whose
  default the `harness.md` addendum settles when the wiring lands.
- One task directory is **one cclens session**. kilocode's own
  task⇔session mapping (`sessions/<id>/session.json`, holding
  `taskSessionMap`) lives outside the tasks tree and is not read — the
  adapter never needs it.
- Timestamps are **milliseconds since the Unix epoch** (13 digits), in both
  files.

## The two files, and the join between them (the crux)

kilocode splits what other harnesses keep in one place. The **conversation**
records what the model did but carries **no usage at all**; the **display
log** carries the usage but no structured conversation. An `Assistant`
record needs both, so the adapter joins the two files in time order — this
join is the kilocode adapter's crux, the way the `message.id` repetition
reconciliation is Claude Code's.

### `api_conversation_history.json` — the record spine

An array of `{role, content, ts, reasoning_details?}` messages; `content`
is an array of blocks in the observed data (a bare string is tolerated as
text material). The blocks map as follows.

| Block | Under | Mapping |
| --- | --- | --- |
| `text` | user message | part of the human turn (prompt material) |
| `tool_result` | user message | the tool-result mirror — never a human turn |
| `tool_use` | assistant message | `ToolUse{tool: name}` |
| `text` | assistant message | display only — no record |
| `reasoning_details` (message key) | assistant message | display only — no record |

- A user message is a `HumanTurn` **when it carries text material** (a
  string or a `text` block) — including the observed case of a user message
  carrying text alongside a `tool_result`. A `tool_result`-only user message
  is the Anthropic-format mirror of a tool result and must not delimit
  spans.
- `ts` orders the file; there is no message id.

### `ui_messages.json` — the token source of record

An array of `{ts, type: say|ask, say?/ask?, text, partial?}` display rows,
of which the adapter reads exactly one kind.

- `say=api_req_started` — its `text` is a JSON string payload
  `{apiProtocol, tokensIn, tokensOut, cacheWrites, cacheReads, cost,
  usageMissing, inferenceProvider}`. Sampled rows carry `usageMissing=false`
  with real tokens; the exception is a request that failed or never
  completed, which stays `usageMissing=true` with no tokens.

Everything else — `say=text` / `reasoning` / `command_output` /
`checkpoint_saved` / `user_feedback`, and the `ask=tool` / `command` /
`command_output` / `followup` / `resume_task` / `completion_result` rows —
is display or interaction furniture, unread in v1 (`ask=tool` is the
approval point; the signals table explains why it is still not a usage
signal).

### The time-ordered join

An `api_req_started` row's `ts` sorts **before** its request's output (the
usage payload describes the request that follows it in time), so the
adapter walks the merged `ts` order of both files.

- Each `api_req_started` row claims the assistant conversation messages
  **strictly between it and the next row** — its *request window*.
- One window can hold **more than one assistant message** (observed: pairs
  6–30 s apart, each carrying a `tool_use` block). The join pairs the row
  with the whole window, never a single message.
- The row yields **one `Assistant` record** — never one per assistant
  message, so there is no repetition to reconcile (the Claude Code
  `message.id` problem does not exist here either)
  - `prompt_size = tokensIn + cacheReads + cacheWrites` (a documented
    assumption, below)
  - `out_tokens = tokensOut`
  - `model = "unknown"` — the payload carries no model name
    (`inferenceProvider` is a provider, never a model; mapping one to the
    other would be a guess). The reporting-honesty label for this missing
    model is a `cli.md` addendum, alongside `sub_tokens_estimated`
- The record is stamped with the window's **last** assistant message `ts` —
  the request's completion point — so the cost record sits *after* its
  window's tool records instead of inverting the stream (the same lesson
  the opencode adapter recorded about creation-time stamping). A window
  holding no assistant message stamps the row's own `ts`: that row is a
  failed or cancelled request (`usageMissing=true`, zero tokens) — the
  request still happened, and it is recorded with zeros, as the opencode
  adapter records an errored request.
- Assistant messages **outside** any window (before the first row) carry no
  cost record — no measured request row, never fabricated.

Record order within a session is `ts` order; within a window the tool
records come **before** that window's `Assistant` cost record, mirroring the
opencode adapter's ordering.

## Usage signals the adapter extracts

| Signal | Where |
| --- | --- |
| Skill invocation | **none observed** — kilocode config installs `skills/` and `command/*.md`, but neither file carries an invocation marker in the samples. The signal stays absent until a marker is measured; a later skills catalog would show live `Skill` usage as orphaned rows in `inventory`, which is the honest missing-site signal |
| Subagent spawn | **none observed** — no spawn marker or join key measured. Subagent runs are not ingested; `sub_tokens`/`sub_agent_count` stay zero and the split reads as unavailable, as under opencode |
| Tool use | `tool_use` blocks in assistant messages (observed: `execute_command`, `read_file`, `apply_diff`, `update_todo_list`, `ask_followup_question`, `attempt_completion`, …) |
| Tool failure | the `tool_result` paired by `tool_use_id` carrying `is_error == true` (1 of 155 sampled blocks — the Anthropic-compatible failure field). Text from the block content; the target from the paired `tool_use` input |
| User prompt | a user message carrying text material |
| Permission denial | **none observed** — `ask=tool` rows appear only where approval was requested (12 rows against 49 tool calls in one sampled task; auto-approved calls emit none), so they undercount and are no usage signal. No denial marker in the samples (0 phrasing hits) |
| Retry | `say=api_req_retry_delayed` — friction input candidate, not mapped in v1 |
| Work events | `tool_use` inputs, the same way the other adapters report work: `execute_command`'s `command` leading word (`bash_cmd`), `apply_diff`/`write_to_file`'s `path` basename with the full path alongside (`file_edit`). Reads (`read_file`) are not work |

`tool_result.tool_use_id` joins each failure back to its exact `tool_use`
block (`id`), the same join Claude Code uses — the failure inherits the
paired tool's name and input, and the text is classified with the shared
`core::friction` vocabulary. A phrasing outside that vocabulary lands in
`Other` with its tool, target, and excerpt intact — never dropped. Teaching
the classifier new phrasings is a cross-harness change (it re-labels
already-ingested sessions, so it needs an `ANALYZER_VERSION` bump) and is
out of scope here.

**`prompt_size` is a documented assumption.** kilocode reports `tokensIn`
plus `cacheReads`/`cacheWrites` but never states whether `tokensIn` already
includes cached tokens. The adapter mirrors the Claude Code and opencode sum
— `prompt_size = tokensIn + cacheReads + cacheWrites` — so
`ctx_growth`/`ctx_start`/`ctx_peak` keep their meaning. If `tokensIn`
already includes cache reads this overstates the floor; the `overhead`
reconciliation (`surfaces.md`) is the honest check, and this paragraph is
where the assumption is versioned when kilocode documents it.

## Project root (a stated heuristic)

Neither file carries a `cwd`-style field. The adapter estimates the root as
the **common prefix of the leading absolute paths** observed in tool inputs
(`path`, `files[]`, also mirrored in the `ask=tool` payloads' `path`), and
states the estimate as a heuristic — its confidence sits below Claude Code's
first-`cwd` root and DSH's `session.cwd`, because it depends on the tools the
task happened to run rather than a field the format guarantees. A task whose
tools name no absolute paths at all yields an unrooted session — `root` stays
empty, as a Claude Code transcript with no `cwd` does — rather than a guess.

## What is deliberately not mapped (v1)

- `cost` (per request) — billing-adjacent and provider-specific; counts are
  usage signals for ranking, not a ledger (`architecture.md`).
- `apiProtocol` / `inferenceProvider` — protocol metadata, not usage.
- `task_metadata.json` (`files_in_context[]`) — future material for the
  never-fires rule; unread until a usage join exists for it.
- `checkpoints/` and the task⇔session `session.json` mapping — unread.
- `ui_messages.json` kinds other than `api_req_started` — display or
  interaction furniture (`ask=tool` is the approval point; the signals
  table explains why it is not a usage signal).
- Compaction — no marker observed in either file; the context metric is
  compaction-safe without it (`events.md`).
- Slash-command invocations typed by the human — no structural wrapper in
  either file, and substring-matching prompts would misread prose discussing
  commands. Config surfaces (`~/.config/kilo/`, commands, skills, MCP,
  custom modes) — the catalog half is a separate spec and a separate change.

## Forward compatibility

Read only the fields above; ignore unknown `say`/`ask` kinds, unknown block
types, and missing optional keys. A new kilocode tool appears as a generic
`ToolUse` (still counted in usage), never as a break. A usage payload that
is missing, unparseable, or marked `usageMissing=true` yields a zero-token
request record — a missing measurement is recorded as zero, never
fabricated, and replace semantics make the in-flight case idempotent on
re-ingest.
