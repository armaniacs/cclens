# What this PR adds: multi-harness support, opencode first

`cclens` analyzes coding-agent usage to show where an agent's configuration can
be optimized — cost (context tokens, wall-clock time, friction) against actual
usage of every configuration surface. Until now it understood only **Claude
Code**: it read Claude Code transcripts, Claude Code config, and its reports
spoke only Claude Code. This PR generalizes the tool to other coding agents,
starting with **opencode**, without changing anything for existing Claude Code
users.

The design keeps the tool configuration-wide: skills are one surface among
many, and now agents are one harness among many.

## The problem

Claude Code's artifacts are hardcoded throughout the original tool.

- Input: session transcripts and config are read from `~/.claude/...` paths in
  Claude Code formats.
- Reports: copy says "Claude Code", suggests `~/.claude`, `CLAUDE.md`, and
  `.claude / CLAUDE.md` regardless of which agent produced the data.
- AI-assisted optimization: `optimize` launches `claude` itself, so the advisor
  cannot run where Claude Code does not.

A user whose sessions live in opencode gets wrong advice: the tool cannot read
the data at all, and the advice it would give points at the wrong files.

## What was added

### A `--harness` flag with per-harness isolation

- `--harness claude` is the default and behaves exactly as before; `--harness
  opencode` switches every command to opencode data.
- Each harness analyzes into its **own SQLite store** (`cclens.db` vs
  `cclens-opencode.db`), so two agents' analyses never blend.
- Reading a store with the wrong harness fails with a hint instead of silently
  mixing data ("the store ... holds opencode sessions — rerun with `--harness
  opencode` (or pick another `--db`)"). Frozen reads are guarded the same way,
  and an unknown value is rejected with the valid set.

### An opencode adapter

A new `src/adapter/opencode.rs` owns all knowledge of opencode's formats,
mirroring the Claude Code adapter's role.

- Sessions come from **opencode's own store** (`~/.local/share/opencode/
  opencode.db`: `session`, `message`, `part` tables), not a transcript file.
  Parts become domain records, then the same normalized events the Claude path
  writes — spans, usage, tool errors, work events — through the unchanged
  intermediate store (`docs/specs/architecture.md`).
- Config surfaces come from opencode's documented locations: global
  `~/.config/opencode` (commands, agents), project `.opencode`, `AGENTS.md`
  with its `@import`s, and **skills from the six documented sites**.
- The parse is lenient where upstream is unversioned: a part shape the adapter
  does not recognize still yields its tool, target, and excerpt instead of
  disappearing. Cost rows are stamped with the turn end so a trailing part
  cannot produce a negative duration.
- Skills dedup by **canonical path** across the six sites, because sites may
  reach one shared directory through symlinks (the observed shape is
  `.opencode/skills -> ../.agents/skills`). Deduping by name instead would
  shadow the shared definition into an "unused" row at one scope — a wedge the
  catalog would manufacture.

### Harness-aware copy in every report

A `Harness` abstraction (src/adapter/harness.rs) carries the words each harness
needs, so reports stop speaking only Claude Code.

- Display name ("Claude Code" / "opencode"), the article ("a" / "an"), the
  instruction file (`CLAUDE.md` / `AGENTS.md`), the project config reference
  (`.claude / CLAUDE.md` vs `.opencode / AGENTS.md`), the global config
  reference (`~/.claude` vs `~/.config/opencode`), and the source noun in the
  cost explanation.
- Stuck-agent lines are agent-neutral: "the agent got stuck re-editing ..." —
  no Claude-specific wording anywhere in opencode output.
- The `optimize` prompt, the briefing, and the guard messages all render the
  same way.

### AI-assisted optimization that launches the harness's own agent

`optimize` now launches **the selected harness's agent**, seeded with the
analysis.

- Claude Code: unchanged — an interactive `claude` session that applies file
  changes only after the user approves the plan.
- opencode: a **headless** `opencode run --format json` session. The prompt
  carries the no-edit mandate (the plan is the agent's final output; no user is
  present to approve anything), and the launch verifies opencode's own
  error message instead of assuming success.

### A live activity indicator for the headless advisor

A headless run used to look hung: quiet stretches (model thinking, a long tool
call) had no signal, and the raw JSON stream was too noisy to watch. The
terminal now shows **one in-place status line**, modeled on Claude Code's
spinner (src/status.rs).

- A turning braille spinner proves liveness, the status names the **current
  activity** from the last tool call ("running shell", "reading",
  "spawning subagent", "thinking"), and an elapsed counter makes waiting
  measurable.
- On a terminal the line rewrites in place instead of scrolling; when stderr
  is piped the same beats fall back to newline logs every 10s, so a pipe never
  receives escape codes. `optimize | pbcopy` still shows the live spinner.
- Text output clears the line before printing, so narration never interleaves
  with the spinner; `NO_COLOR` and `TERM=dumb` are respected; error paths stop
  the indicator through `Drop`.
- The narration and the final plan go to stdout (pipe-friendly); every raw
  line and the advisor's stderr go to a private `0600` log whose path is
  printed up front. `--verbose` restores the raw stream for debugging.

### Hardening found in a real-data trial

Every piece was verified against a real opencode store (91 sessions across 8
projects). The trial surfaced four fixes, all shipped here.

- Recurring **failure categories from real errors**: `binary-file` (an
  encoding mismatch, not a missing file) and `web-error` (a fetched URL
  answering 404). Six of eight previously unclassified failures became named,
  actionable rows.
- A catalog directory that **exists but cannot be read warns** instead of
  reading as empty. macOS gates `~/Documents` behind a per-app permission, so
  a project there silently scanned as "no skills configured" and every live
  surface read as orphaned usage. A missing directory stays silent (the scope
  may simply not exist); any other read failure prints one stderr warning
  naming the directory.
- **Honest attribution** for the always-on context: the floor minus global
  config is not all "the harness itself" — most of it is the user's own skill
  descriptions and `AGENTS.md`, and the copy now says so (or says the config
  estimate covers the floor instead of printing a "0"). The prompts view no
  longer tells an analyzed store to "run `analyze` first" when the harness
  harvests no prompt signal.

## What deliberately stayed out

- **DSH and kilocode adapters.** The `--harness` flag and the store isolation
  are ready for them, but their session/config formats are not pinned yet —
  an adapter written against guessed formats would measure noise.
- **opencode MCP surfaces.** No MCP definition was present in the observed
  config and no MCP-namespaced tool appeared in the usage sample, so the
  server-to-tool join cannot be pinned down.
- **opencode permission surfaces.** The permission table is real, but no
  runtime denial signal joins it, so every entry would read "unused" — a
  false wedge.
- **Global `AGENTS.md` under opencode.** Whether opencode loads one is
  unverified, so only the project file is scanned.

Each exclusion is documented with its reason in
`docs/specs/config-format-opencode.md`.

## Compatibility

- Without `--harness` (or with `--harness claude`), behavior is unchanged:
  same inputs, same reports, same `optimize`.
- The shared core — records to events to aggregation, and the surface-usage
  join — is harness-agnostic; harness knowledge lives only in the adapter
  layer (`docs/specs/architecture.md`).
- The store schema gains no migration: each harness simply writes its own
  database file.

## How it is verified

- The full gate passes: `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings`, and `cargo test` — **304 tests** (247 before
  this branch).
- Every report view, the guards, and both `optimize` launch modes were
  exercised against a real opencode store (91 sessions, 8 projects), and the
  flag-less Claude Code path was re-verified on a real Claude store (367
  sessions re-analyzed, unchanged behavior).
- Design intent for each piece is documented spec-first: `docs/specs/
  harness.md` (the `optimize` contract under opencode, report copy naming the
  harness), `docs/specs/session-format-opencode.md` (the session store
  mapping), `docs/specs/config-format-opencode.md` (config surfaces and the
  reasoned exclusions), and updates throughout `docs/specs/cli.md` and
  `docs/specs/surfaces.md`.
- The exploration that led to the design — including the kilocode findings —
  is in `docs/exploration/other-harness.md`.

## Commit map

The 18 commits build the feature in layers, each green at its own gate.

- `1910e1f`, `2ba235b`, `4211c9c` — exploration notes (formats, decisions,
  kilocode token findings), English rewrite
- `422d4f2`, `36694fc` — opencode session parsing into domain records;
  turn-end stamping so trailing parts cannot go negative
- `2c7cab7` — `--harness` flag, store isolation, analyze under opencode
- `515c9fd` — opencode config catalog (commands, agents, `AGENTS.md`)
- `f3508e3` — classify observed opencode error phrasings
- `c5a8579` — harness-aware copy in reports and briefings
- `1746587`, `7e54fa8`, `7092a60`, `6a4f410` — headless `optimize` under
  opencode: launch, quiet terminal, nested-part rendering fix, live activity
  indicator
- `de3b628`, `cd59550`, `8369fee`, `7ef7e71` — real-data trial hardening:
  new failure categories, the six skills sites with canonical dedup, the
  unreadable-directory warning, honest attribution
