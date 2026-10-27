# saniwalens

A lens onto your Claude Code usage. `saniwalens` reads your session transcripts and
live configuration, extracts them into a local SQLite store, and shows **where
time, tokens, and effort are being wasted — and how to fix it**: unused or heavy
config, recurring tool failures, where the work gets stuck, and the cost picture
behind it all.

The design intent lives in [`docs/specs/`](docs/specs/); this README is how to
run it.

## Install / build

With Homebrew (macOS and Linux, Intel and arm):

```sh
brew install armaniacs/saniwalens/saniwalens
```

With Cargo, from [crates.io](https://crates.io/crates/saniwalens) (builds from
source; bundled SQLite needs a C compiler):

```sh
cargo install saniwalens
```

With Nix (no toolchain to set up):

```sh
nix run github:armaniacs/saniwalens -- doctor   # run without installing
nix profile install github:armaniacs/saniwalens  # install `saniwalens` onto PATH
nix profile install github:armaniacs/saniwalens/vX.Y.Z  # a specific release
```

From source — dev tools are pinned by the Nix flake, tasks run via `just`:

```sh
nix develop          # enter the dev shell (pinned rust, just, sqlite, …)
just                 # list tasks
just check           # rustfmt --check + clippy (the CI gate)
just test            # cargo test
just build           # release binary at target/release/saniwalens
```

Plain Cargo also works (`cargo build --release`) if you have a recent stable
Rust; the flake just pins it. CI runs `nix develop -c just check` / `just test`.

Publishing a GitHub Release for a `vX.Y.Z` tag builds binaries for Linux, macOS,
and Windows (x86_64 and arm64 each), attaches them to that release with
`.sha256` files, points the
[`armaniacs/homebrew-saniwalens`](https://github.com/armaniacs/homebrew-saniwalens)
tap at the new archives, and publishes the crate to crates.io. The release is
published before its assets are built, so the notes are visible for a few
minutes with nothing attached yet.

The tag is the only place a version is written down: the release workflow
rewrites `version` in `Cargo.toml` from it before building and publishing, so
the committed `0.0.0` is a placeholder and cutting a release never means
bumping a file first. The flake stamps it too, without being told the tag:
`VERSION` is marked `export-subst`, so the archive GitHub serves for the tag
carries the tag in it (see
[nix-github-release](https://github.com/lambdalisue/nix-github-release)).

The tap covers Linux and macOS only; on Windows, take the `-pc-windows-msvc`
archive for your arch from the release, or build from source. `CLAUDE_CONFIG_DIR`
names the config tree directly when it is set to a non-empty path, as it does
for Claude Code itself. Otherwise — including when the variable is set but
empty — `~/.claude` is located through `HOME`, falling back to `USERPROFILE`
where Windows leaves `HOME` unset.

## Claude Code plugin

The repository is also a Claude Code plugin marketplace
([`.claude-plugin/`](.claude-plugin/) + [`plugins/saniwalens/`](plugins/saniwalens/)),
so you can drive saniwalens from inside a Claude Code session:

```
/plugin marketplace add armaniacs/saniwalens
/plugin install saniwalens@saniwalens
```

| Skill | What it does |
| --- | --- |
| `/saniwalens:doctor` | Analyze and present the one-screen health check in the session. |
| `/saniwalens:optimize` | The `saniwalens optimize` advisor, in the **current** session: investigate the findings to root cause and propose concrete config fixes. |
| `/saniwalens:query` | Answer an ad-hoc usage question with read-only SQL over the store. |

The skills use the `saniwalens` binary when it is on PATH and fall back to
`nix run github:armaniacs/saniwalens` otherwise. They read and write the same
per-user store as the CLI.

## Quick start

```sh
# 1. Start here — a one-screen health check, split into what to fix globally
#    (~/.claude) vs per project. Analyzes automatically on first run.
saniwalens doctor

# 2. Narrow to the layer you're optimizing.
saniwalens doctor --scope global
saniwalens doctor --scope project:<slug>

# 3. Act on the findings: hand them to an interactive `claude` session that
#    investigates each root cause and proposes concrete fixes.
saniwalens optimize --scope global
```

Analysis reads your transcripts (`~/.claude/projects`) and live config —
`~/.claude/*` plus each analyzed project's own `.claude/` / `CLAUDE.md` —
**read-only** and incrementally, writing the store. Nothing is sent anywhere;
the store is a local file. Every command takes `--db <path>`; it defaults to
`${XDG_STATE_HOME:-~/.local/state}/saniwalens/saniwalens.db` — one per-user store,
because what is analyzed spans every project, not the directory you run from.
Pass `--db ./saniwalens.db` if you deliberately want a project-local one (and
`.gitignore` it). Read commands refresh the store automatically before reporting
(unchanged transcripts are skipped, so this is fast) and print a freshness line
on stderr; pass `--frozen` to read the store exactly as it is. Running
`saniwalens analyze` yourself is only needed to refresh without reading.

## Other agent harnesses

The same lens works on opencode and Kilo Code transcripts via `--harness`:

```sh
saniwalens analyze --harness kilocode      # reads the editor's globalStorage task files
saniwalens doctor --harness kilocode
saniwalens optimize --harness kilocode     # opens an interactive `kilo --prompt` session
```

Each harness keeps its own store (`saniwalens-opencode.db`, `saniwalens-kilocode.db`)
and reads its own config — `~/.config/opencode/`, `~/.config/kilo/` plus each
analyzed project's own files — so findings stay per harness, never mixed. The
flag is explicit and the default stays `claude`; [`docs/specs/harness.md`](docs/specs/harness.md)
holds the per-harness contract.

## Commands

### Pipeline

| Command | What it does |
| --- | --- |
| `saniwalens doctor` | The health check, split global / per-project — **start here**. |
| `saniwalens analyze` | Extract transcripts + config into the store (incremental). Reads run it automatically. |
| `saniwalens sql '<query>'` | Run an arbitrary read-only SQL query against the store (see below). |
| `saniwalens optimize` | Hand the findings to an interactive `claude` session to act on. |

`--scope global | project | project:<slug>` (on `doctor`, `failures`,
`waste`, `inventory`, `optimize`) restricts a report to the config layer being
optimized: an unused/heavy config finding routes by where that file is
installed, and a recurring failure belongs to the project that owns a strict
majority of it — otherwise it is a cross-project habit and shows up as global.

### Views

Each is a curated lens that carries logic a raw query cannot (a classification,
an algorithm, a suggestion). Add `--format markdown` to paste into a PR or
note, or `--format json` (every command, `doctor` and `analyze` included) for
typed machine-readable output — stdout carries nothing but the JSON, since all
freshness chatter goes to stderr. Human output is colorized on a terminal;
`NO_COLOR` or piping turns it off.

```sh
saniwalens waste                 # ranked optimization opportunities, with a suggested action
saniwalens waste --scope global  # …only what a global-config cleanup should touch
saniwalens inventory               # every installed surface (with its scope) × its actual usage
saniwalens overhead               # always-on context per session, reconciled vs your config
saniwalens usage                  # per-skill usage, most-invoked first
saniwalens usage --by month       # usage per time bucket (JST: year|month|week|day|hour)
saniwalens failures               # recurring tool failures by category, ranked, with fixes
saniwalens failures --scope global          # failures no project owns (a cross-project habit)
saniwalens failures --scope project:<slug>  # one project's failures (fix lands in its config)
saniwalens prompts                # how you steer the session (steer/correct/question/instruct)
saniwalens stuck                 # files Claude got stuck re-editing in rapid bursts
```

### `sql` — query the store directly

Anything the curated views don't cover is a SQL query — the store is the whole
point. The query is an argument, or read from **stdin**:

```sh
# Which tool produces each failure category?
saniwalens sql "SELECT category, tool, COUNT(*) n FROM tool_errors GROUP BY 1,2 ORDER BY n DESC"

# The actual failing paths behind path-not-found (stdin form):
echo "SELECT excerpt FROM tool_errors WHERE category='path-not-found'" | saniwalens sql

# Discover the schema:
saniwalens sql "SELECT sql FROM sqlite_master"
```

The `tool_errors` view names the failure columns (`category`, `excerpt`,
`tool`, `project`); the db is opened read-only, so a query can never mutate the
derived store.

## What it covers

- **Every configuration surface** — skills, rules, agents, MCP servers,
  `CLAUDE.md` — catalogued with its startup and on-demand token costs and load
  mode, joined
  against actual usage to flag unused (delete), always-on heavy (slim), and
  costly+rare (trim).
- **Where the work stumbles** — recurring tool failures by category and
  originating tool (`failures`), files Claude got stuck re-editing (`stuck`),
  each with concrete examples and the project it concentrates in.
- **Where tokens go** — main-thread skill output vs. subagent cost, the subagent
  half split per agent type (which agents actually spent it, nested spawns
  included), and the always-on context floor reconciled against your readable
  config (the residual is system + tools + MCP you cannot trim from files).
- **How you prompt** — the steer / correct / question / instruct mix.

## Notes

- Counts are usage signals for ranking, **not a billing ledger**; static cost is
  a token estimate, not a measured runtime figure.
- The store outlives your transcripts. Claude Code prunes those on its own
  retention schedule, and saniwalens keeps the rows it already extracted — so the db
  holds history that `~/.claude/projects` no longer can, and deleting it is a
  real loss, not a cache flush. That is why it lives under
  `${XDG_STATE_HOME:-~/.local/state}/saniwalens/` rather than a cache directory a
  cleaner may empty. Upgrading saniwalens across a schema change migrates the file
  in place; you never need to delete it.
- `optimize` launches the harness's own interactive session (`claude` by
  default, `kilo --prompt` under `--harness kilocode`) seeded with the
  findings; the briefing is written to a private temp file, never passed on
  the command line.
- Not yet implemented (see [`docs/specs/`](docs/specs/)): meta-skill (`loop`)
  nesting — a pending design decision on detecting parent/child from the
  transcript.
