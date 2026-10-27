# What this PR adds: the tool is `agentlens`, and the legacy store moves with it

`cclens` was named when the tool understood only Claude Code — a Claude Code
lens. The multi-harness support made that name wrong twice over: the tool now
reads opencode and kilocode data too, and crates.io holds the `cclens` name
already. This PR renames everything the name touches — the crate and binary,
the CLI command, the per-harness store files, the temp-file prefixes, the
docs — to **`agentlens`**, except the legacy directory itself, which moves
rather than gets left behind.

## The store migration

The store outlives the transcripts it was parsed from (`storage.md`): Claude
Code prunes those on its own schedule, and the db keeps the rows it already
extracted. A rename that only changed where the binary looks would silently
orphan that history — the new binary would start a fresh store and the old
one would sit unread beside it.

So the first open under the new name moves the pre-rename directory
(`~/.local/state/cclens/`) to the new one and renames its per-harness files
(`cclens.db` → `agentlens.db`, `cclens-opencode.db`, `cclens-kilocode.db`):

- **Never deleted** — a move, not a cleanup; the history survives the rename.
- **Never overwrites** — a store already living under the new name stays, and
  the legacy one survives beside it rather than being removed to make room.
- **An explicit `--db` is taken as is** — the rename is not ours to apply to
  a path the user chose themselves.

## What moved

- `Cargo.toml`: the package name, the repository (the fork is now the
  canonical home), the description (the tool is agent-generic), the keywords.
- `src/cli.rs`: the clap command name, the default store directory
  (`~/.local/state/agentlens/`), the temp-file prefixes, the legacy-store
  move with its tests.
- `src/adapter/harness.rs`: the per-harness db file names.
- `src/store.rs` / `src/core/optimize.rs`: the copy that names the tool
  ("written by a newer agentlens", the `agentlens sql` walkthrough lines).
- `README.md`, `docs/specs/`, `docs/pr/`, `.github/workflows/`, `flake.nix`:
  the install paths (`cargo install agentlens`,
  `brew install armaniacs/agentlens/agentlens`,
  `nix run github:armaniacs/agentlens`) and the name in prose. The only
  `lambdalisue` reference left is `nix-github-release` — a separate tool the
  release workflow reuses, not this project.

## After this PR

- `cargo install agentlens` / `agentlens doctor --harness kilocode`.
- The GitHub repository, the Homebrew tap (`armaniacs/homebrew-agentlens`),
  and the crates.io publication follow outside this PR.
