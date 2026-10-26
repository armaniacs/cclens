# What this PR adds: a kilocode harness, stacked on multi-harness support

`cclens` analyzes coding-agent usage to show where an agent's configuration can
be optimized. The stacked base PR (`feature-otherharness`) generalized the tool
to other coding agents, starting with opencode, via a `--harness` flag with
per-harness isolation. This PR adds **kilocode** (Kilo Code / the `kilo` CLI)
as the second harness under the same flag — `--harness kilocode` — with no
change to claude or opencode behavior.

## The problem

A user whose sessions live in Kilo Code gets nothing: the tool cannot read
kilocode's data at all. And kilocode's transcripts live in a place the
Claude Code path never looks — the editor's `globalStorage` task files, not
`~/.claude/projects` transcripts and not opencode's SQLite store.

## What was added

### A kilocode adapter

A new `src/adapter/kilocode.rs` owns all knowledge of kilocode's formats
(`docs/specs/session-format-kilocode.md`), mirroring the opencode adapter's
role.

- Sessions come from the **editor's globalStorage task files** (`Library/
  Application Support/Code/User/globalStorage/kilocode.kilo-code/tasks/<uuid>/`):
  `api_conversation_history.json` (the Anthropic-shaped messages) plus
  `ui_messages.json` for tokens. Fingerprinting is per task (`kilocode:<uuid>`,
  max mtime + summed size), so unchanged tasks are skipped on re-analyze.
- Tokens join from `ui_messages.json`'s `say=api_req_started` rows
  (`tokensIn/tokensOut/cacheWrites/cacheReads/cost/usageMissing`), time-ordered;
  one session-start row per window is the overhead floor.
- Tool failures are read as the **readable message the tool carried**: the
  measured `tool_result` wraps the failure in a JSON envelope inside a text
  block followed by an `environment_details` decoration, and storing the
  joined blob whole mis-classified failures twice over (the envelope's "does
  not exist" matched the path vocabulary; the decoration's file names matched
  timeout). Each block unwraps, the decoration drops, and the category comes
  from the same text the excerpt carries.
- Sessions root at the first absolute `cwd` an `execute_command` carried —
  measured to hold exactly one distinct value per task — so 68 of 92 tasks
  root to one project instead of noise.

### A kilocode config catalog

`docs/specs/config-format-kilocode.md` catalogues the config half, observed
against a real `~/.config/kilo/` tree and a real project: commands, agents,
the skills sites (deduped by canonical path), the global and project
`AGENTS.md`, `.kilorules`, and the files in `kilo.json`'s `instructions[]` —
kilo's own addition, the measured way a project pointed the array at a
gitignored `AGENTS.local.md` that is otherwise invisible to the catalog while
costing tokens on every session. MCP and permissions stay out of v1: kilocode
emitted zero `mcp__` tool calls and has no denial marker to join, so either
catalog would manufacture an unused wedge. Measured on the real store: 83
surfaces catalogued, and `waste`/`inventory` join against them.

### `optimize` opens an interactive kilo session

`--harness kilocode optimize` launches `kilo --prompt <pointer>` — the
interactive session the claude launch gives, with the user present to approve
the briefing read (kilo gates every tool call where opencode does not, so a
headless run would need `--auto` instead — measured working, and measured
dying without it). The exercise found and fixed three real defects in cclens
itself: the excerpt defect above, an unknown tool name mis-classified as
path-not-found ("unknown tool" joined the tool-not-available vocabulary), and
work events leaving `started_at` empty while carrying the epoch — a shared
store defect, not kilocode-specific.

## Commits

- `29d5830` — kilocode task-file parsing (spec `session-format-kilocode.md`)
- `aebf8a1` — `--harness kilocode` analyze wiring
- `7b3cf65` — root sessions at `execute_command`'s cwd
- `6d3d599`, `65d81f3`, `73bbf6b`, `01ee28b` — the optimize launch: refusal,
  the kilo launch wiring, the headless path, then the interactive session
- `81b80ea`, `186575a`, `d483232` — real-data walkthrough hardening: readable
  timestamps, honest tool-name labels, the excerpt fix, the findings recorded
- `5bf22d1`, `996db74` — the config catalog (spec, then implementation)
- `97aa71a`, `44cd78c`, `b2ba261` — lint, the honest-absence note, the README
