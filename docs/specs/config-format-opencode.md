# Config Format Specification — opencode

This spec catalogues what the **adapter layer** knows about opencode **config
files** — the catalog half for the opencode harness. Its Claude Code sibling
is `config-format.md`; the join semantics are shared (`surfaces.md`). Only the
adapter uses the paths and shapes below. Like every upstream-tracking spec,
this one describes what was observed and marks what was not.

> Observed against a real `~/.config/opencode/` tree (opencode `1.x`). No
> project-level `.opencode/` directory and no MCP definition were present in
> this environment — the project layout and the MCP site below follow the
> documented conventions and are marked as such until a live tree confirms
> them.

## Where config lives

Config exists at two scopes; project overrides/extends global. The adapter
scans both and records the scope on each surface — `Scope::Project` carries
the owning project's directory, since many projects coexist in one store
(`harness.md`: `project` is the session's `directory` verbatim).

| Surface kind | Global | Project-local |
| --- | --- | --- |
| `command` | `~/.config/opencode/command/*.md` | `<project>/.opencode/command/*.md` |
| `agent` | `~/.config/opencode/agent/*.md` | `<project>/.opencode/agent/*.md` |
| `claude_md` | — (unverified; not scanned) | `<project>/AGENTS.md` (+ its `@import`s) |

`~/.config` throughout means `$XDG_CONFIG_HOME` when that variable names an
absolute path, else `~/.config` — the XDG rule, mirroring how the store
resolution treats `XDG_STATE_HOME`.

Which project roots get scanned is driven by the *sessions*, exactly as for
Claude Code: `analyze` collects the distinct session directories and scans
each root that still exists on disk. A deleted root contributes no project
surfaces.

## Static cost and load mode

Both surface kinds load like Claude Code skills: only the frontmatter
`description` is startup-listed, the body waits for invocation. So each
surface records the whole file as `static_tokens` (its invocation cost) and
the description alone as `startup_tokens`, under
`LoadMode::StartupDescription`. The token estimator is the shared
`approx_tokens` (roughly four characters per token) — a ranking signal, not
a billing figure.

**The startup-listing is an assumption**, stated here so a wrong table entry
can be found: it was not verified against opencode's prompt construction the
way the Claude Code table was pinned to release `2.1.x`. If opencode lists
only names (not descriptions), every `startup_tokens` here overstates by the
description weight; the `overhead` reconciliation against the observed
session-start floor is the honest check.

`description` is read defensively (plain/quoted/block scalars with folded
continuation lines); a file declaring none costs zero at startup. Unknown
frontmatter keys are ignored.

`AGENTS.md` reuses the shared `claude_md` weighing (whole text,
`StartupFull`, `@import` files as their own surfaces) — opencode reads the
same file the Claude Code adapter weighs, so the cost lands identically.
`CLAUDE.md` is deliberately **not** scanned: opencode's documented
instruction file is `AGENTS.md`, and cataloguing a file the harness never
loads would invent always-on cost.

## Identity and the join

- `command` surfaces take the file stem as id (`review.md` → `review`).
  Commands are **catalog-only**: a human invoking `/review` leaves no
  structural marker in the transcript (user text is prose, and
  substring-matching it would misread discussions *about* commands), so no
  event can ever join them. The core already treats unknown kinds as
  catalog-only (`is_usage_measurable` lists only measurable kinds), which is
  why no core change backs this spec — absence of events is never read as
  disuse.
- `agent` surfaces take the file stem as id and join `agent_spawn` events by
  the `task` tool's `subagent_type`. Built-in agents (`build`, `explore`,
  …) exercised without a matching file surface appear as **orphaned usage**,
  the mirror of unused config — reported, not hidden.
- A surface existing at both scopes under one `(kind, id)` resolves per
  project (project shadows global), exactly as for Claude Code.

## Deliberately out of v1

- **Skills.** `tool=skill` invocations join a `skill` surface, but no skill
  definition site was verified (no global `skills/` dir here, no documented
  project path confirmed against a live tree). Cataloguing an invented path
  would either miss real files or weigh decorative ones — so v1 catalogs no
  `skill` surfaces, and live `Skill` usage surfaces as orphaned usage in
  `inventory`, which is the honest signal that a catalog site is missing.
- **MCP servers.** No MCP definition was present (`opencode.json` carries no
  `mcp` key here), and no MCP-namespaced tool appeared in the usage sample,
  so the server↔tool join cannot be pinned down. Skipped until both halves
  are observed together.
- **`permission` surfaces.** The `opencode.json` permission table is real,
  but no runtime denial signal joins it (see `session-format-opencode.md`),
  so every entry would read "unused" — a false wedge manufactured by the
  catalog, not found in usage. Skipped until a denial marker is found.
- **Global `AGENTS.md`.** Whether opencode loads one is unverified; only the
  project file is scanned.
