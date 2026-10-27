//! agentlens CLI entry point. See `docs/specs/cli.md`.

fn main() -> anyhow::Result<()> {
    agentlens::cli::run()
}
