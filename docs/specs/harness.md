# Harness Specification

saniwalens analyzes sessions from more than one agent harness (Claude Code,
opencode; kilocode and DSH to follow). This spec owns the **harness contract**:
how the CLI selects a harness, where each harness's input and store live, and
what each harness can and cannot report in v1. The per-harness *formats* live
in their own specs (`session-format.md`, `session-format-opencode.md`); the
*analysis semantics* stay shared (`events.md`, `surfaces.md`).

## One store per harness

Stores are **never shared** across harnesses. Each harness gets its own SQLite
file holding a single harness's sessions, so the Claude Code path — behavior,
defaults, and existing stores — is untouched by every later harness:

| Harness | Default store (`--db`) |
| --- | --- |
| `claude` | `${XDG_STATE_HOME:-~/.local/state}/saniwalens/saniwalens.db` (unchanged) |
| `opencode` | same state dir, `saniwalens-opencode.db` |
| `kilocode` | same state dir, `saniwalens-kilocode.db` |

An explicit `--db` always wins, exactly as before. The schema is identical in
every store (`storage.md`); only the rows' origin differs, which is why no
`harness` column exists — a file holding one harness needs no per-row label,
and a shared file is what this design refuses to create. No cross-harness
aggregation exists in v1.

Every store records its harness in `meta` (`harness = claude | opencode |
kilocode`).
A command reading a store built by another harness **refuses with a hint**
naming the expected `--harness` (or a matching `--db`), rather than reporting
one harness's rows as another's. `sql` enforces the same guard: it resolves
its default `--db` through `--harness`, so `saniwalens --harness opencode sql`
lands on the opencode store without flags.

## `--harness`

A global flag, default `claude`:

```
saniwalens [--harness claude|opencode|kilocode] <command>
```

- The default keeps every existing invocation byte-identical in behavior —
  this is what keeps the change upstream-mergeable.
- There is deliberately **no `auto`**: detection order across harnesses would
  be ambiguous (several harnesses installed, several stores present), and an
  ambiguous default misattributes rows silently. Explicitness is the whole
  point of separate stores.
- Unknown values fail with the valid list.

`--projects` keeps its meaning ("input root") but resolves per harness:
a directory for Claude Code (`$CLAUDE_CONFIG_DIR/projects`), the opencode
database file for opencode (default `~/.local/share/opencode/opencode.db`),
and the kilocode tasks directory for kilocode — default the **first existing
editor globalStorage candidate** (`kilocode_tasks_candidates`), since the
path varies with the editor install (Code / Cursor / VSCodium) and the
platform, so the candidate list, not one hardcoded path, is the contract.
The resolved root is recorded in `meta` (`projects_dir`) and reused by the
read commands' auto-refresh, unchanged plumbing carrying a new value.

## opencode analyze (v1 capability)

`--harness opencode analyze` reads the opencode database **read-only** and
ingests it into the opencode store. The mapping itself is
`adapter::opencode` (`session-format-opencode.md`); this section is the shell
contract:

- **Sessions** come from the `session` table in `time_created` order.
  `project` is the session's `directory` verbatim (real paths need no slug
  recovery), `root` is the same directory, `slug` is opencode's own slug,
  and the ingest delete-key (`source_path`) is `opencode:<session-id>`.
- **Incremental ingest is per session**, keyed `opencode:<session-id>` with
  `(mtime = session.time_updated, size = message count)`. A whole-file
  fingerprint would re-ingest every session on each run (opencode appends
  constantly); per-session keys skip what did not move. Replace semantics
  make re-ingest idempotent, as for Claude Code transcripts.
- **Invalidation has its own version key.** The shared `ANALYZER_VERSION`
  would force Claude Code users through a full re-analyze on every
  opencode-only extraction change, so opencode rows stamp
  `meta.opencode_analyzer_version` instead
  (`adapter::opencode::OPENCODE_ANALYZER_VERSION`). Bump it under the same
  rule: whenever opencode extraction changes what an unchanged database
  yields.
- **Subagent runs are not ingested.** opencode has no subagent-side
  transcripts, so `sub_tokens`/`sub_agent_count` stay zero and the per-agent
  split is empty. `SplitCoverage::is_unavailable` requires a positive total,
  so no misleading "run analyze to fill it" warning fires — zero subagent
  activity reads as zero, which is what it is.
- **Prompts are not ingested in v1.** The store's prompt pointer is a
  `(source_path, source_line)` file coordinate (`storage.md`); opencode text
  lives in database rows, so the pointer needs a new encoding. Until then the
  `prompts` view is empty under opencode — reported as empty, never
  fabricated.
- **Permission denials count zero.** No runtime denial marker was found in
  the opencode format, and no heuristic stands in for one.
- **The surface catalog covers commands, agents, and `AGENTS.md**. Global
  `command/` and `agent/` come from the opencode config root; each known
  project root contributes its `.opencode/` pair plus `AGENTS.md` (with
  `@import`s). Skills, MCP servers, and permissions stay out until their
  definition sites and usage joins are both observed
  (`config-format-opencode.md`); live `Skill` usage meanwhile shows as
  orphaned rows in `inventory`, which is the honest signal that a catalog
  site is missing.
- `meta.config_dir` is recorded empty: no config was scanned. (An empty
  value, not a missing key, so readers can distinguish "nothing scanned"
  from "unknown".)

## kilocode analyze (v1 capability)

`--harness kilocode analyze` reads each task directory's two JSON files
**read-only** and ingests it into the kilocode store. The mapping itself is
`adapter::kilocode` (`session-format-kilocode.md`); this section is the shell
contract:

- **Sessions come from the task directories** under the resolved input root,
  sorted for a deterministic order. A directory without both
  `api_conversation_history.json` and `ui_messages.json` is not a session and
  is skipped. One task directory is one saniwalens session; the task uuid is both
  the session id and the slug.
- **The project root is a stated heuristic** (`estimate_root`): the common
  parent directory of the **leading** tool paths, since kilocode stamps no
  `cwd` anywhere. A task whose tools name no absolute path stays unrooted —
  empty, never guessed.
- **Per-task ingest keys.** `kilocode:<task-uuid>` keys each task, with
  (mtime = the later of the two files', size = their summed length) as the
  fingerprint — either file moves on activity, and the directory stat would
  re-ingest every task on each run (checkpoints move too). Replace semantics
  make re-ingest idempotent, as for the other harnesses.
- **Invalidation has its own version key**, `meta.kilocode_analyzer_version`
  (`adapter::kilocode::KILOCODE_ANALYZER_VERSION`), for the same reason the
  opencode rows carry theirs: a kilocode-only extraction change must never
  force other harnesses through a full re-analyze.
- **Subagent runs, prompts, and permission denials are not ingested.** No
  spawn marker, no usable prompt pointer (the store's `(source_path,
  source_line)` coordinate has no line meaning in JSON), and no denial marker
  was measured in the kilocode format — the counts stay zero and the views
  read as empty, which is what they are.
- **The surface catalog is empty.** The global `~/.config/kilo` sites are
  measured (commands, skills, `kilo.json`) but not cataloged yet — the config
  half is a separate spec and change. `inventory` says so instead of showing
  a table that looks like an empty config, and `meta.config_dir` is recorded
  empty (nothing scanned, distinguishable from unknown).

## `optimize` under opencode

The briefing composes from the store either way, so `--print` works under
every harness. Launching runs the harness's own agent on a data-free pointer
prompt (the briefing travels via a private temp file, as for Claude Code):
`claude <prompt>` opens interactively, while `opencode run --format json
--title 'saniwalens optimize' <prompt>` runs the advisor headless — no user is
present for the approval gate, so the headless instructions
(`run_instructions`) make the printed plan the final output and forbid
edits.

The headless terminal stays quiet: saniwalens reads the JSON event stream line
by line and surfaces only what the agent says — text parts (narration, then
the plan) on stdout, one-line tool markers on stderr — while every raw line
and the child's stderr go to a private (`0600`) log file whose path is
printed up front. Rendering is lenient (the upstream event shapes are
unversioned): a line with no recognizable text stays in the log, and a run
that renders nothing readable says so instead of leaving silence.
`--verbose` inherits stdio for the raw streaming instead.

Under kilocode the briefing composes from the store either way, so `--print`
works. Launching opens an **interactive kilo session** on the pointer prompt —
`kilo --prompt <prompt>`, the experience the claude launch gives: the user is
present to approve the briefing read (kilo gates every tool call where
opencode does not, so a headless run would need `--auto` instead), steer the
investigation, and apply the fixes themselves. A bare `kilo <text>` positional
is treated as a file to open (measured), not a prompt — `--prompt` is the
TUI's message surface.

## Report copy names the harness

Copy that attributes rows to a harness uses `Harness` display helpers, so
opencode output never claims to be Claude Code's:

- `display_name` in the `doctor` heading, the always-on residual line, and
  the `prompts` framing; the `stuck`/`thrash` lines say "the agent" (neutral
  for both harnesses).
- `instruction_file` / `project_config_ref` / `global_config_ref` wherever a
  finding routes to the config layer owning the fix (`doctor`, `failures`,
  `inventory` framing, `overhead` breakdown, friction suggestions).
- The `optimize` briefing and advisor instructions render from a
  `HarnessCopy` (`core::optimize`), plain data the shell assembles from the
  harness — the core never names a harness.
- Freshness lines count the harness's own unit (`transcript(s)` vs
  `session(s)`).

Static `--help` texts still describe the Claude Code defaults; per-harness
help generation is out of scope.

## Forward compatibility

A new harness adds: a `Harness` variant, its default input root and store
name here, its format spec, its adapter module, and its `run_analyze_*`
ingestion. Nothing in the Claude Code path moves — that immobility is the
contract this spec protects.
