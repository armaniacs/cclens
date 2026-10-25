# What this PR adds: a live activity indicator for the headless advisor

This PR lands on top of the multi-harness work (`feature-otherharness`) and
improves one slice of it: how a headless `optimize` run looks while it works.

`cclens optimize --harness opencode` launches opencode's own agent headlessly,
seeded with the analysis. The terminal stays quiet by design — the narration
and the plan go to stdout, raw output goes to a private log. The quiet run
already prints a dim heartbeat line every 30s during quiet stretches, so a run
never looks completely hung. But 30s is a long time to stare at silence, and
the heartbeat says nothing about *what* the advisor is doing right now.

## What changes

The heartbeat becomes a **single in-place status line**, modeled on Claude
Code's activity spinner.

- A turning braille spinner proves liveness. The frame turns every 100ms —
  fast enough to read as alive, slow enough that a screenshot still shows a
  clean glyph.
- The status names the **current activity**, taken from the last tool event:
  "running shell", "reading", "searching", "spawning subagent", "planning",
  and "thinking" after text output. Unmapped tools read as "running \<name\>"
  rather than a guess.
- An elapsed counter (`(42s)`) makes waiting measurable.

```text
⠸ advisor working — running shell (42s)
```

## Why in-place, and why the pipe fallback matters

- On a terminal the line rewrites in place (`\r` + `ESC[2K`) instead of
  scrolling, so a long run does not flood the screen. The redraw holds a
  shared lock with the clear routine, so narration printed between redraws
  never interleaves with the status line.
- When stderr is piped, ANSI rewrites would corrupt the stream — the same
  beats fall back to newline logs every 10s (`… advisor working — running
  shell (42s, 41 events)`), and per-tool progress markers print as lines. A
  pipe never receives escape codes.
- Piping **stdout** keeps the live spinner: `optimize | pbcopy` still shows
  the status line on the terminal while `pbcopy` receives only the narration
  and the plan.
- Text output clears the line before printing, so the narration never prints
  on top of the spinner.
- `NO_COLOR` and `TERM=dumb` are respected; the indicator stops through
  `Drop` on error paths, so a failed launch cannot leave a thread or a stale
  line behind.

## What stays the same

- Every raw event still goes to the run log — the indicator only decides what
  the terminal shows.
- The 10s newline beats replace nothing on a piped stderr: quiet stretches are
  covered there too, with the same "waiting, not hung" guarantee.
- `--verbose` still inherits stdio for the raw stream.

## How it is verified

- Unit tests cover the spinner frame cycling, the activity-label mapping, the
  status-line format, and the symlink-level details of the shared state.
- The full gate passes: `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings`, and `cargo test` — **304 tests**.
- Exercised against a real opencode store (91 sessions): the status line
  rewrites in place on a terminal, falls back to newline beats when stderr is
  piped, and `| pbcopy` receives only the narration and the plan.

## Commit

- `6a4f410` — feat(status): live activity indicator for the headless advisor
