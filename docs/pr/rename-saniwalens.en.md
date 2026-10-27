# What this PR adds: the tool is `saniwalens`, and the legacy move chains

The previous rename PR picked `agentlens` — and turned out to be wrong twice
over: crates.io holds that name already, published in January by a tool in the
**same domain** (`nguyenphutrong/agentlens`, "CLI tool to prepare codebases for
AI agents"), so the collision is real and searching the registry would
misdirect users. The availability check that cleared the name was faulty: the
command that built the sparse-index path malformed the URL and read every
candidate as free.

This PR renames everything the name touches to **`saniwalens`** — free on the
registry, the index, and GitHub, with no repository or code match anywhere.

## The legacy move now chains

The binary has been renamed twice (`cclens` → `agentlens` → `saniwalens`), so
a user's store can live under either earlier name. The first open under the
current name moves whichever legacy directory exists — the intermediate
`agentlens` or the original `cclens` — renamed under the new name with its
per-harness files:

- **Never deleted** — a move, not a cleanup.
- **Never overwrites** — a store already living under the current name stays.
- **An explicit `--db` is taken as is.**

## Also

- The `version = "0.5.0"` a hand-publish attempt stamped onto `Cargo.toml` is
  back to the `0.0.0` placeholder — the tags are the record, the committed
  version is never what ships.
- The release formula's class name is generated from the tool name
  (`class Saniwalens < Formula`), and the tap repository renames with it
  (`armaniacs/homebrew-saniwalens`).
