# What this PR does: drop the plugin system, generalize the product copy

Follow-up to the renames. Two cleanups the rename surfaced:

- **The Claude Code plugin system is removed, not renamed.** saniwalens is
  multi-harness and Claude Code is not its main stage — Claude-only users are
  served by the original cclens. Shipping a Claude-only plugin from this repo
  would mislead, so the marketplace manifest, the `plugins/cclens` pack, the
  README plugin section, and the dead Cargo exclude entries go away.
- **Product copy stops assuming Claude Code.** AGENTS.md (the agent
  entrypoint), the README lead and coverage list, the crate docs, the flake
  description, and the architecture spec's one-question line now say
  coding-agent. Claude-specific references stay where they belong: the Claude
  harness's display name, paths, and adapter specs — plus the legacy store
  names in the migration code and its tests.
