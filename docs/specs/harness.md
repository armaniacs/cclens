# Harness Specification

cclens analyzes sessions from more than one agent harness (Claude Code,
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
| `claude` | `${XDG_STATE_HOME:-~/.local/state}/cclens/cclens.db` (unchanged) |
| `opencode` | same state dir, `cclens-opencode.db` |

An explicit `--db` always wins, exactly as before. The schema is identical in
every store (`storage.md`); only the rows' origin differs, which is why no
`harness` column exists — a file holding one harness needs no per-row label,
and a shared file is what this design refuses to create. No cross-harness
aggregation exists in v1.

Every store records its harness in `meta` (`harness = claude | opencode`).
A command reading a store built by another harness **refuses with a hint**
naming the expected `--harness` (or a matching `--db`), rather than reporting
one harness's rows as another's. `sql` enforces the same guard: it resolves
its default `--db` through `--harness`, so `cclens --harness opencode sql`
lands on the opencode store without flags.

## `--harness`

A global flag, default `claude`:

```
cclens [--harness claude|opencode] <command>
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
database file for opencode (default `~/.local/share/opencode/opencode.db`).
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
- **The surface catalog is empty in v1** (`replace_surfaces(&[])`).
  Config scanning is the next change (`config-format-opencode.md`, not yet
  written). Until then `inventory`/`waste` report no rows and `doctor`
  omits the pruning section — the usage views (`usage`, `failures`,
  `stuck`, `overhead`, `sql`) are fully functional.
- `meta.config_dir` is recorded empty: no config was scanned. (An empty
  value, not a missing key, so readers can distinguish "nothing scanned"
  from "unknown".)

## `optimize` under opencode (v1)

The briefing composes from the store either way, so `--print` works under
every harness. *Launching* composes a `claude` session, which is wrong for
an opencode setup — so a non-Claude harness without `--print` refuses with
a message saying so, instead of seeding the wrong agent. (The briefing's own
advisor preamble still says Claude Code in v1; rewording it per harness
follows with the view-copy pass.)

## Report copy names the harness

Copy that attributes rows to a harness uses `Harness::display_name`, so
opencode output never claims to be Claude Code's (v1 covers the `doctor`
heading and the always-on residual line; deeper view copy still says
Claude and follows later).

## Forward compatibility

A new harness adds: a `Harness` variant, its default input root and store
name here, its format spec, its adapter module, and its `run_analyze_*`
ingestion. Nothing in the Claude Code path moves — that immobility is the
contract this spec protects.
