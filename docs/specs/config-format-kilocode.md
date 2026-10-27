# Config Format Specification — kilocode

This spec catalogues what the **adapter layer** knows about kilocode **config
files** — the catalog half for the kilocode harness. Its Claude Code sibling
is `config-format.md`; its opencode sibling is `config-format-opencode.md`
(the join semantics are shared, `surfaces.md`). Only the adapter uses the
paths and shapes below. Like every upstream-tracking spec, this one describes
what was observed and marks what was not.

> Observed against a real `~/.config/kilo/` tree and a real project tree
> (kilo CLI `7.3.41`). Kilo is an OpenCode fork, so the layout mirrors
> opencode's under kilo's own names — `~/.config/kilo/` for global and
> `.kilo/` for project — with one addition: kilo's `kilo.json` carries an
> `instructions[]` array of always-on instruction files.

## Where config lives

Config exists at two scopes; project overrides/extends global. The adapter
scans both and records the scope on each surface — `Scope::Project` carries
the owning project's directory, since many projects coexist in one store
(`harness.md`: `project` is the session's `directory` verbatim).

| Surface kind | Global | Project-local |
| --- | --- | --- |
| `command` | `~/.config/kilo/command/*.md` | `<project>/.kilo/command/*.md` |
| `agent` | `~/.config/kilo/agent/*.md` | `<project>/.kilo/agent/*.md` |
| `skill` | `~/.config/kilo/skills/`, plus the shared `~/.claude/skills/` and `~/.agents/skills/` | `<project>/.kilo/skills/`, plus the shared `<project>/.claude/skills/` and `<project>/.agents/skills/` |
| `claude_md` | `~/.config/kilo/AGENTS.md` | `<project>/AGENTS.md` (+ its `@import`s), `<project>/.kilorules`, and every file in `kilo.json`'s `instructions[]` |

`~/.config` throughout means `$XDG_CONFIG_HOME` when that variable names an
absolute path, else `~/.config` — the XDG rule, mirroring how the store
resolution treats `XDG_STATE_HOME`.

Which project roots get scanned is driven by the *sessions*, exactly as for
Claude Code: `analyze` collects the distinct session directories and scans
each root that still exists on disk. A deleted root contributes no project
surfaces.

**The `instructions[]` array is kilo's own addition** and the reason it is
catalogued at all: it is how `kilo.json` loads always-on instruction files —
the measured project pointed it at a gitignored `AGENTS.local.md`, and the
measured global config at an absolute path into the Claude Code tree
(`~/.config/claude/RTK.md`). A file only reachable through this array is
otherwise invisible to the catalog while costing tokens on every session —
the exact blind spot an optimization advisor needs named. Paths resolve
against the config file's directory at the global scope and the project root
at the project scope; absolute paths pass through.

## Static cost and load mode

Commands and agents load like Claude Code skills: only the frontmatter
`description` is startup-listed, the body waits for invocation. Each records
the whole file as `static_tokens` (its invocation cost) and the description
alone as `startup_tokens`, under `LoadMode::StartupDescription`.

**The startup-listing is an assumption**, stated here so a wrong table entry
can be found: it was not verified against kilo's prompt construction. If kilo
lists only names, every `startup_tokens` here overstates by the description
weight; the `overhead` reconciliation against the observed session-start floor
is the honest check.

Instruction files (`AGENTS.md`, `.kilorules`, `instructions[]` entries) reuse
the shared `claude_md` weighing: whole text as `static_tokens` and
`startup_tokens`, `LoadMode::StartupFull`, `@import` files as their own
surfaces. `.kilorules` is the project rules file kilo loads always-on
(measured present in a project that used kilo heavily); treating it as an
instruction file rather than a `rule` reflects how kilo loads it — always,
not per-path.

The token estimator is the shared `approx_tokens` (roughly four characters
per token) — a ranking signal, not a billing figure.

## Identity and the join

- `command` surfaces take the file stem as id. Catalog-only, as under
  opencode: a human invoking a command leaves no structural marker in the
  transcript (user text is prose), so no event can ever join them. The core
  already treats unknown kinds as catalog-only, so no core change backs this
  spec.
- `agent` surfaces take the file stem as id. kilocode yields **no
  `agent_spawn` events** (measured across 92 tasks: zero), so every custom
  agent file reads as unused config — a catalog-only surface in practice,
  reported honestly rather than hidden.
- `skill` surfaces join `tool=skill` events by the skill name, the same join
  Claude Code uses. Sites may reach one shared directory through symlinks, so
  surfaces dedup by **canonical path** — one row per actual definition, no
  matter how many sites reach it. But kilocode maps **no skill-invocation
  signal** in v1 (see `session-format-kilocode.md`), so every skill reads
  `uses 0` / `UNUSED` under kilocode — the same honest absence as the agent
  rows: reported, not evidence of disuse, and the waste view's suggestion
  stays "declutter only" because the startup saving is the description alone.
- A surface existing at both scopes under one `(kind, id)` resolves per
  project (project shadows global), exactly as for Claude Code.

## Deliberately out of v1

- **MCP servers.** The `mcp` key is real (measured: a single `skillport`
  entry), but kilocode emitted **zero** `mcp__`-namespaced tool calls across
  92 tasks — the server↔tool join has no usage half to pin against. Skipped
  until both halves are observed together.
- **`permission` surfaces.** The `kilo.json` permission table is real
  (measured: `bash` patterns and `external_directory` paths), but kilocode's
  auto-approved calls emit no denial marker (see
  `session-format-kilocode.md`), so every entry would read "unused" — a false
  wedge manufactured by the catalog, not found in usage. Skipped until a
  denial marker is found.
- **`agent` entries inside `kilo.json`.** The measured keys are the stock
  modes (`plan`, `debug`, `ask`, `code`) — product defaults, not user
  config. Custom agents are files under `agent/`, which is what is catalogued.
- **`provider`, `model`, `commit_message`, and the other `kilo.json` keys.**
  Settings, not cost surfaces.
- **`.kilo/agents/`, `.kilo/plugins/`, `.kilo/plans/`.** Present in the
  measured project tree but not the documented convention (`.kilo/agent/` is);
  whether kilo loads them is unverified, and cataloguing files the harness
  never loads would invent always-on cost.
