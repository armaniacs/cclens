//! saniwalens CLI entry point. See `docs/specs/cli.md`.

fn main() -> anyhow::Result<()> {
    saniwalens::cli::run()
}
