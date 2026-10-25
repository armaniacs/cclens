# Multi-harness exploration (opencode / kilocode / DSH)

- Branch: `feature-otherharness` (this file records the exploration work).
- Goal: reshape cclens from Claude Code-only into something usable with
  opencode, kilocode, and DSH.
- Method (per AGENTS.md): spec-first, TDD, strict privacy — no real data is
  pasted here. Everything below summarizes shapes read read-only from local
  directories; ids, paths, and prompt text are synthetic stand-ins.
- House rule for this branch: all docs and code stay in English; conversation
  with the author stays in Japanese.
- Date: 2026-10-25 JST.

## 1. Where the Claude Code assumptions live (what to peel off)

| Location | Claude-specific content |
| --- | --- |
| `src/adapter/transcript.rs` | JSONL `type` (`assistant`/`user`/`system`), `message.usage.*`, `tool_use` in `message.content[]`, special-cased `Skill`/`Agent`, `<command-name>`, `toolDenialKind`, the `subagents/` + `*.meta.json` linkage, first-`cwd` root, `promptId` |
| `src/adapter/config.rs` | `skills/*/SKILL.md`, `rules/**/*.md`, `agents/*.md`, `settings.json`, `mcp.json` under `~/.claude` (`$CLAUDE_CONFIG_DIR`); project-side `.claude/` + `CLAUDE.md`/`AGENTS.md` (with `@import` tracking) |
| `src/cli.rs` (around `run_analyze`) | `--projects` defaults to `$CLAUDE_CONFIG_DIR/projects`, config root defaults to `~/.claude`, project roots enumerated from session `cwd` values |
| `src/store.rs` | No harness column on `sessions`. `events.kind` uses Claude-born vocabulary (`skill_invocation`, `agent_spawn`, …). `ANALYZER_VERSION = "3"` (must bump when extraction semantics change) |
| `docs/specs/*` | `session-format.md` / `config-format.md` catalogue the Claude formats directly. The `format-isolation.md` boundary (only the adapter knows upstream shapes) stays reusable as-is |

## 2. Measured input shapes per harness (read-only survey)

### 2.1 opencode — closest fit. SQLite is the source of record

- Session source of record: `~/.local/share/opencode/opencode.db` (SQLite, drizzle schema)
  - `session(id, project_id, directory, title, agent, model, tokens_input/output, cost, time_created/updated)`
  - `message(id, session_id, time_created, data JSON)` — `data.role` / `data.tokens{input,output,reasoning,cache{read,write}}` / `data.cost` / `data.modelID,providerID,agent,mode` / `data.path{cwd,root}` / `data.error{...}`. `parentID` links reply chains
  - `part(id, message_id, session_id, data JSON)` — observed `data.type` distribution (~5000 rows): `tool` / `step-start` / `step-finish` / `text` / `reasoning` / `compaction` / `file` / `subtask`
  - `step-finish` carries `tokens{total,input,output,cache}` + `cost`. Compaction is directly detectable via `type=compaction` (the `compact_boundary` equivalent)
- File mirror: `project/global/storage/session/{info, message}/...` holds the same content as JSON, but the DB joins more cheaply. No `subagents/`-style split exists (agent runs are `part.tool=task` / `part.type=subtask`)
- Usage-signal mapping:
  - Skill invocation → `part.tool=skill` (`input.name` is the skill name). opencode has nothing like Claude's slash/tool split, so a single `source=tool` suffices
  - Subagent spawn → `part.tool=task` / `part.type=subtask` (keeps `agent`, `command`, `prompt`)
  - Tool use → `part.tool=<name>` (`read`/`bash`/`edit`/`write`/`glob`/`grep`/`webfetch` …). `callID` is the `tool_use_id`-equivalent join key
  - Tool failure → `part.state.status=error` + `state.error` (e.g. `File not found: /tmp/example/foo.md`). Same slot as Claude's `is_error`/`tool_use_error` wrapper. `input.filePath`/`command` becomes the target
  - Permission denial → `opencode.json`'s `permission` table (allow/ask/deny) is static evidence. No runtime denial marker found (needs follow-up survey)
  - User prompt → `message.role=user` + `part.type=text`. `session.title` is a generated summary, never a prompt
- Config side:
  - Global: `~/.config/opencode/opencode.json` (`model`, `small_model`, `permission`, `providers`, …) + `plugins/`. No `command/` or `agent/` directories in this environment (needs cross-checking against the project-level `.opencode/command/*.md` convention)
  - Project: no repo with `.opencode/` / `opencode.json` observed here. `AGENTS.md` is a shared convention opencode also reads, so the existing `claude_md` surface (symlink dedup included) is reusable
  - MCP: definition site unverified. This environment's `opencode.json` has no top-level `mcp` key

### 2.2 kilocode (Roo-code family) — under VSCode globalStorage. Token source of record is `ui_messages.json`

- Sessions: `<VSCode globalStorage>/kilocode.kilo-code/tasks/<uuid>/`
  - `api_conversation_history.json` — Anthropic-compatible: `{role, content:[{type:text|tool_use|tool_result, id/name/input, tool_use_id/content}], ts, reasoning_details?}`. `tool_use.name=execute_command` etc. `tool_use_id` linkage recovers tool and target (same shape as Claude Code, easy to reuse). **Zero `usage`/`tokens`/`cost` keys measured — tokens are not here**
  - `ui_messages.json` — `{ts, type:say/ask, say:text/...}` display log. Usable for human-turn delimiters (span end conditions). **The token source of record is here**: `say=api_req_started` rows carry JSON text `{apiProtocol, tokensIn, tokensOut, cacheWrites, cacheReads, cost, usageMissing, inferenceProvider}` (49 of 50 sampled rows have `usageMissing=false` with cost). They sort just before their `assistant` replies in `ts` order, so a time-ordered join yields per-request rows. No model name is carried (`inferenceProvider` only), so the representative model stays NULL
  - `task_metadata.json` — measured keys are `files_in_context[]` only (read/edited traces). Future material for never-fires-rule
  - Task⇔session mapping: `sessions/<id>/session.json` holds `taskSessionMap` (taskId → sessionId)
  - Project root: no `cwd`-style key in messages. `ask=tool` text JSON carries `{tool, path, content: <absolute path>, isOutsideWorkspace}`, so the common prefix of leading absolute paths is the root-estimation candidate (stated as a heuristic)
  - Approval signal: `ask=tool` is the tool-execution approval point (permission equivalent). Failure-signal candidates: `say=command_output` (carries exit codes) and `api_req_retry_delayed`
- Config side (measured):
  - Global: `~/.config/kilo/` — `command/*.md` (`devloop.md`, `graphify.md`, `review-diff.md` = slash-command equivalents), `skills/*/` (skill directories), `kilo.json` (providers/models, per-agent prompts), `agent/` (empty in this environment). Plus `settings/mcp_settings.json` (`{mcpServers:{}}`) and `settings/custom_modes.yaml` (empty here)
  - Project-level conventions (`.kilocode/rules/` etc.) not observed in this environment. Needs follow-up survey
- Paths are strongly environment-dependent (`~/Library/Application Support/Code/...` varies with the VSCode/Cursor installation), so the adapter wants a storage-root override (env var or `--projects`) — resolved in the v1 wiring as `Harness::kilocode_tasks_candidates` (Code → Cursor → VSCodium × platform dirs, first existing wins), with `--projects` overriding
- `kilo` CLI (measured, for the `optimize` launch question): `/opt/homebrew/bin/kilo` (Homebrew shim → `Cellar/kilo/7.3.41/libexec/kilo`, Bun Mach-O binary), version 7.3.41. It validates its config on every start (`~/.config/kilo/kilo.json` — an unrecognized key fails the whole start), and keeps state under `~/.local/state/kilo` and `~/.local/share/kilo/state`. Its prompt-launch surface (whether it takes a positional prompt like `claude <prompt>`) is still unmeasured, which is why the v1 `optimize` launch is deferred

### 2.3 DSH (DeepSeek Harness) — home-grown event sourcing. zstd-compressed JSONL

- Sessions: `~/.dsh/sessions/<cwd-slug>/<session-id>/session.v4.jsonl.zstd`
  - One event per line, `{type, seq, time, data}`. Measured type distribution: `tool/call` / `tool/result` / `step/start` / `assistant/message` / `step/end` / `job/registered` / `job/settled` / `user/message` / `agent/inbox/spliced` / `session/title` / `session` / `permission/preset` / `sandbox/mode` / `approval/policy` / `turn/start` / `system/message` / `request/header` / `request/context` (+ `session/title-llm-request`)
  - Origin: `type=session` carries `{version:4, id, createdAt, cwd, agentPreset}`. Same confidence as Claude's first-`cwd` root, with none of the slug lossiness
  - Tool calls: `{type:tool-call, id, name, arguments}` inside `assistant/message` `content[]`, plus `tool/call{callId,name,arguments}` → `tool/result{content,isError}`. `callId` is the join key. Observed tool names: `bash`, `read`, `web_search`, … (DSH-native verbs)
  - Tokens: `usage{inputTokens,outputTokens,totalTokens,cacheReadTokens?}` confirmed (hosting event still to pin down — look near `source.replayState` on `assistant/message` and near `step/end` next)
  - User prompt: `user/message{content[], source{kind:user}}`. Needs the same guard as Claude's `isMeta`/`tool_result` exclusion
  - Subagents: no `subagent/` split. Planned detection via `delegationDepth` and the `task`-family tool (still to identify)
- Config side: `~/.dsh/profiles/<name>/` (+ workspace files like `cordis.yml`). Skill/agent/MCP definition sites unidentified — needs follow-up survey. The shared `AGENTS.md` convention is reusable

### 2.4 Cross-cutting: shared vs harness-specific

- Shareable: the `AGENTS.md`/`CLAUDE.md` always-on surface (with symlink dedup and `@import` tracking), `Scope` (global/project), `approx_tokens` static cost, the catalog×usage join, wedges, and reporting-honesty thinking
- Harness-specific: the full "where and what shape" of sessions, skill-invocation markers, subagent join keys, token field names, config locations, permission/denial vocabulary → all confined under the adapter (an extension of format-isolation)

## 3. Design (with decisions. Raised as spec diffs once agreed)

Decisions (2026-10-25):
1. **opencode is the first target** — SQLite source of record plus real tokens gets value out earliest
2. **No shared store across harnesses** — with upstream merge in mind, Claude's default behavior and default DB (`cclens.db`) stay untouched. Each harness gets its own DB file (`--db` defaults switch per harness, e.g. `cclens-opencode.db` under the same state dir; exact names settled when the spec is drafted). `meta` stays per-DB. No cross-harness aggregation in v1
3. **kilocode got its follow-up survey** — reflected in §2.2. The token source of record (`api_req_started`) is confirmed, so the NULL-operation plan is withdrawn in favor of a time-ordered join producing per-request rows
4. **kilocode v1 wiring landed (2026-10-25)** — one task dir = one cclens session (`kilocode:<task-uuid>` ingest keys, mtime = the later of the two files', size = their sum), its own version key `meta.kilocode_analyzer_version` (apart from the shared `ANALYZER_VERSION`), the project root from the `estimate_root` heuristic, `cclens-kilocode.db` under the same state dir, and `inventory` reading as "not implemented yet" instead of "run analyze first". The `optimize` launch stays deferred until the `kilo` prompt surface is measured; the config catalog is a separate spec and change

- Adapter split: `adapter::{claude,opencode,kilocode,dsh}` with the shared domain model (`Record`, `Surface`, `SubagentRun`) unchanged. The existing `adapter::{config,transcript}` bodies move to `adapter::claude::{config,transcript}` with re-exports left on the old paths (prevents boundary bugs). DB separation makes a `sessions.harness` column **unnecessary** — each DB holds a single harness
- CLI: `--harness claude|opencode|kilocode|dsh` on `analyze`/`doctor`/views/`optimize` (default `claude` = current behavior, unchanged. No `auto` — avoids inventing ambiguous detection order). Per-harness default tables for `--projects`/`--config`/`--db` (opencode DB path, kilocode tasks dir, `~/.dsh/sessions`, plus per-harness DB names). Record `harness` in `meta` and warn when a DB is read under the wrong harness
- kilocode's missing model: treat the unmappable representative model as a reporting-honesty label (spec `cli.md` addendum, alongside `sub_tokens_estimated`)
- `optimize`: the launch command differs per harness (`claude` / `opencode run` / …). Unify the `--print` briefing first; keep the launch Claude-compatible and defer the rest

## 4. Difficulty order and TDD plan

Status: opencode is fully landed (mapping spec, pure adapter commit `422d4f2`, CLI wiring, config catalog, copy naming). kilocode transcript mapping spec + pure adapter done (`session-format-kilocode.md`, `adapter::kilocode`), and the kilocode CLI wiring is landed (item 4 + part of item 5). Remaining:

1. opencode CLI wiring (`--harness opencode`, DB-reading shell, per-harness `--db` default, `meta.harness` guard) — first for user-visible value
2. opencode config catalog (`config-format-opencode.md`, then catalog implementation) — completes `waste`/`inventory`
3. DSH adapter (zstd JSONL decode → records; pinning the `usage` host event is the crux)
4. kilocode adapter (records plus the `api_req_started` time-ordered token join)
5. doctor/optimize harness handling + harness addenda to `docs/specs/{session-format,config-format,storage,cli}.md`

## 5. Open survey items

- [ ] opencode: runtime denial signal for `permission`, MCP definition site, `.opencode/command|agent` convention
- [x] kilocode: token source of record → `ui_messages.json` `say=api_req_started` text JSON (`tokensIn/tokensOut/cacheWrites/cacheReads/cost/usageMissing`). Storage-root choice resolved in the v1 wiring (`kilocode_tasks_candidates`: Code → Cursor → VSCodium, first existing). Still open: project-side rules/modes locations
- [ ] DSH: hosting event for `usage`, subagent-spawn markers, skill/agent/MCP definition sites
- [ ] General: default per-harness DB file names (settled when the harness spec is drafted)
