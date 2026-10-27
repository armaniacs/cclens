# What this PR does: stamp the binary version from git, generalize keywords

- `saniwalens --version` printed the `0.0.0` placeholder for everyone
  building from source. A new `build.rs` asks git (`git describe --tags`,
  leading `v` stripped) and stamps it at compile time, falling back to the
  package version where git cannot answer — so the binary names its release
  everywhere, not just in stamped artifacts.
- `keywords` drops harness-specific `claude` for `coding-agent`, matching the
  multi-harness positioning. Takes effect from the next publish.
