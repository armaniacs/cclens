# What this PR adds: period filtering for the report views (--since / --until / --last)

`saniwalens` analyzes coding-agent usage to show where an agent's configuration can
be optimized. The store keeps the full history of every session and event, but the
report views always aggregated all of it — there was no way to ask "what did the
last 7 days cost" or "did my config change help". This PR adds `--since`,
`--until`, and `--last` to the eight report views, narrowing every aggregate to a
time window.

Also on this branch: the brainstorming candidate ledger that produced this PBI
(`pbi/2026-10-30-00-backlog.md`), and an AGENTS.md rule pinning implementation to
subagent-driven development (a fresh implementer subagent per task, a task review
after each, and a broad whole-branch review at the end).

## The problem

The store keeps history (that is why re-analyze is incremental), but the reports
could not select a period: `usage` bucketed by time (`--by`) yet could not bound
it, and `waste` / `inventory` evaluated "unused" over the whole history. A config
change's effect was invisible — "did the failures drop after I changed X" had no
answer. ccusage (the closest tool) answers it with `--since` / `--until` /
`--last`; saniwalens had nothing.

## What was added

### A pure period core

`src/core/period.rs` parses the three flags into a `TimeRange` of UTC epochs.
`--since` / `--until` take `YYYY-MM-DD`, interpreted in the presentation zone
(JST by default — the same convention as the bucket labels, so an event at
21:00 UTC belongs to the next JST day); `--until` includes the whole day.
`--last <N>d|h|w` is a rolling window from an injected `now`, exclusive with
`--since` / `--until` at the clap level and again inside `resolve`.
`since > until` is an error — an empty report would read as "no usage".

### Range threading through the store

Sixteen aggregation methods take the range. Two helpers (`range_clause` for the
events' `started_epoch`, `range_clause_utc_text` for the uniform RFC 3339 UTC
text column) are the single place the window becomes SQL, so no query branches on
the flags itself. The inventory's catalog half stays unfiltered — the catalog is
current config, only its usage half narrows, and "unused" reads as "unused within
the window". Unstamped sessions are excluded from windowed session figures and
still count without flags (spec'd in `docs/specs/cli.md`).

### Disclosure in the three channels

A drawn window is stated on the stderr freshness line (all formats), in the table
framing block (table only — markdown stays a bare table), and as a `window` key
in `--format json` (existing keys untouched). Without flags nothing changes:
every disclosure helper is gated on a drawn window, and the no-flag output is
byte-identical to the previous binary.

### Unchanged on purpose

`sql` is the raw escape hatch (write your own WHERE), `analyze` keeps reading all
history, and `optimize` takes no flags — its briefing keeps full-history totals
(pinned by a flag-rejection test).

## Verification

- `cargo test` — 418 passed, 0 failed: 13 unit tests in `core::period` (JST day
  boundaries, month end, rolling windows, parse errors), integration tests
  against a real in-memory store for the range threading (inclusive `--until`,
  the millisecond-fraction straddle, catalog rows surviving a windowed LEFT JOIN),
  and a straddle test pinning the two-axis subagent figures.
- `cargo clippy --all-targets --all-features -- -D warnings` and
  `cargo fmt --check` — clean.
- Byte-identity: no-flag output across 60 capture files (8 views, table and json,
  stdout / stderr / exit code) is byte-identical to the previous binary.
- Review trail: subagent-driven (implementer → task review → final whole-branch
  review → fix → re-review, all approved). The final review's Important finding —
  a session straddling `--until` made the subagent split-coverage note claim a
  false reason — is fixed with window-gated wording, the two time axes left
  un-unified, and pinned by the straddle test.
