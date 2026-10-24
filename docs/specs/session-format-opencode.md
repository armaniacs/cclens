# Session Format Specification — opencode

This spec catalogues what the **adapter layer** knows about the **opencode
session** format — the usage half for the opencode harness. Its Claude Code
sibling is `session-format.md`; the two specs share the same downstream
contract (the internal domain model, `core::span::Record`) but describe
different upstream shapes. Only the adapter may use the field names below
(`.claude/rules/format-isolation.md`). The opencode store is **separate** from
the Claude Code store (one DB per harness), so nothing here constrains the
Claude Code mapping, and vice versa.

> Observed against a real `~/.local/share/opencode/opencode.db` (opencode
> `1.x`, SQLite via drizzle) plus its file mirror under
> `project/global/storage/session/`. Counts drift as sessions accumulate; the
> **shapes** are what matter. The database is the source of record — the file
> mirror carries the same content and is not read.

## Where sessions live

```sql
-- ~/.local/share/opencode/opencode.db
session(id, project_id, directory, title, agent, model,
        tokens_input, tokens_output, cost, time_created, time_updated, ...)
message(id, session_id, time_created, time_updated, data JSON)
part(id, message_id, session_id, time_created, time_updated, data JSON)
```

- `session.directory` is the **real working directory** — no lossy slug
  encoding, so unlike Claude Code there is nothing to recover from record
  fields. Project-config scanning walks these directories directly.
- All timestamps are **milliseconds since the Unix epoch** (13 digits), UTC.
  `message.time_created` orders the transcript; `part` rows carry their own
  `state.time{start,end}` where the tool ran.
- `message.data` and `part.data` are JSON blobs. The adapter reads only the
  fields below and ignores the rest; unknown `type` values are skipped.

## Records the adapter reads

### `message` — one row per turn

`data.role` is `user` or `assistant`.

Assistant (`role == assistant`) carries the whole turn's cost in one place:

- `data.tokens{input, output, reasoning, cache{read, write}}` — verified
  identical to the same message's `step-finish` part (`total ==
  input+output+reasoning`), and at most one `step-finish` part exists per
  message. So **message-level tokens are authoritative** and the adapter never
  sums `step-finish` rows — there is no multi-line repetition to reconcile
  (the Claude Code `message.id` problem does not exist here).
- `data.modelID` (e.g. `preview/GLM-5.3-Flash`) — the representative model.
  Absent → `"unknown"` (honest, never a guess).
- `data.agent` / `data.mode` (e.g. `build`) — the running agent.
- `data.path{cwd, root}` — per-message working directory (informational; the
  session root comes from `session.directory`).
- `data.error{name, data{message, statusCode, ...}}` — a failed request
  (e.g. provider `APIError`). Still yields its record with whatever tokens it
  carries (usually zero); the error half is friction input, not a reason to
  drop the row.
- `data.parentID` — the reply-chain link. Used as the coarse spawn key
  (below), the way `promptId` is for Claude Code.

User (`role == user`) rows hold `part` children of type `text` (the prompt)
and `file` (attachments). A user message yields a `HumanTurn` **only when it
has a `text` or `file` part** — `compaction` parts also hang under user
messages and must not delimit spans.

`session.title` is a generated summary, not a prompt — never read as one.

### `part` — one row per content block or tool call

`data.type` decides the mapping:

| `type` | Mapping |
| --- | --- |
| `text` (under user message) | part of the human turn (prompt material) |
| `text` / `reasoning` (under assistant message) | display only — cost already on the message row; no record |
| `tool` with `tool == "skill"` | `SkillInvocation{skill: input.name, source: Tool}` |
| `tool` with `tool == "task"` | `AgentSpawn{agent: input.subagent_type, tool_use_id: callID, prompt_id: message.parentID}` |
| `tool` with any other `tool` | `ToolUse{tool}` (observed: `read`, `bash`, `edit`, `write`, `glob`, `grep`, `todowrite`, `question`, `webfetch`, …) |
| `tool` with `tool == "skill_use"` | **ignored** — this loads skills into context (`input.skill_names`), it is not an invocation. Counting it would double-count `tool == "skill"` rows |
| `type == "subtask"` | **ignored (child-side mirror)** — carries `{agent, command, model, prompt}` for a spawn whose parent-side `task` part already emitted the `AgentSpawn`. Emitting both would double-count agent usage |
| `type == "step-start"` / `"step-finish"` | structural/cost echo — message tokens already counted; no record |
| `type == "compaction"` | compaction marker (`{auto, overflow, tail_start_id}`); the context metric is compaction-safe without it (`events.md`); no record |
| `type == "file"` | attachment pointer; no record |

Record order within a session is `message.time_created` (ties broken by
`message.id`), and within a message the part-derived records come **before**
that message's `Assistant` cost record — so a skill span opens at the
invocation and includes the calling turn's cost, mirroring the Claude Code
adapter's ordering. Part timestamps use `state.time.start`, falling back to
the owning message's `time_created`.

## Usage signals the adapter extracts

| Signal | Where |
| --- | --- |
| Skill invocation | `part.tool == "skill"`, `input.name` |
| Subagent spawn | `part.tool == "task"` (`input.subagent_type`, `callID`); coarse key `message.parentID` |
| Tool use | `part.tool` (any other name) |
| User prompt | user message with a `text`/`file` part |
| Tool failure | `part.state.status == "error"`, text from `state.error`. Target from the input's first present key of `filePath`, `file_path`, `path`, `command`, `pattern`, `url` (opencode spells it `filePath`) |
| Permission denial | **none observed** — `opencode.json`'s `permission` table (allow/ask/deny) is static config with no runtime marker found. No heuristic fallback is invented; the signal is absent until a marker is found |
| Compaction | `part.type == "compaction"` (informational only) |

Failure text is classified with the shared `core::friction` vocabulary, which
was tuned on Claude Code phrasings. An opencode phrasing outside that
vocabulary (e.g. `Cannot read binary file`) lands in `Other` with its tool,
target, and excerpt intact — never dropped. Teaching the classifier new
phrasings is a cross-harness change (it re-labels already-ingested Claude Code
sessions, so it needs an `ANALYZER_VERSION` bump) and is out of scope here.

**`prompt_size` is a documented assumption.** opencode reports `input` plus
`cache{read, write}` but never states whether `input` already includes cached
tokens. The adapter mirrors the Claude Code sum — `prompt_size = input +
cache.read + cache.write` — so `ctx_growth`/`ctx_start`/`ctx_peak` keep their
meaning. If opencode's `input` already includes cache reads this overstates
the floor; the `overhead` reconciliation (`surfaces.md`) is the honest check,
and this paragraph is where the assumption is versioned when opencode
documents it.

## Subagent linkage (v1 scope)

A `task` spawn is exact on the parent side (`callID`). There is **no
subagent-side transcript** — opencode runs subagents in-session, and the
`subtask` part is the child-side mirror, not a cost ledger. Per-agent cost
attribution (`subagent_runs`) is therefore **out of scope for v1**: spawns
count agent-surface usage, and session-level totals come from the `session`
row's `tokens_*`. A future change that finds per-run costs joins on `callID`
/ `metadata.sessionId` without disturbing this mapping.

## What is deliberately not mapped (v1)

- Slash-command invocations typed by the human (`/review` etc.) — user text
  has no structural command wrapper (unlike Claude Code's `<command-name>`),
  and substring-matching prompts would misread prose discussing commands.
- `skill_use` and `subtask` parts (above) — context loading and child-side
  mirrors, not invocations.
- `data.cost` (per message, per `step-finish`) — billing-adjacent and
  provider-specific; counts are usage signals for ranking, not a ledger
  (`architecture.md`).
- Config surfaces (`opencode.json`, commands, agents, MCP) — the catalog half
  is a separate spec (`config-format-opencode.md`, not yet written) and a
  separate change.

## Forward compatibility

Read only the fields above; ignore unknown `type`/`tool` values and missing
optional keys. A new opencode tool appears as a generic `ToolUse` (still
counted in usage), never as a break. `tool == "invalid"` (the model called a
tool that does not exist) maps to `ToolUse{tool: "invalid"}` — it is a model
mistake worth counting, not a parse failure.
