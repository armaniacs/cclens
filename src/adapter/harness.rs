//! Harness selection: which agent harness a run analyzes and where its
//! files live. This module owns the per-harness *locations* (upstream
//! conventions), never the formats themselves. See `docs/specs/harness.md`.

use std::path::{Path, PathBuf};

/// The agent harness one store holds. One store never mixes harnesses, so
/// this labels a whole run — never a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Opencode,
}

impl Harness {
    /// Parse `--harness`. `None` for anything else, so the caller can fail
    /// with the valid list.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Harness::Claude),
            "opencode" => Some(Harness::Opencode),
            _ => None,
        }
    }

    /// The `meta.harness` value and the error-hint vocabulary.
    pub fn label(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Opencode => "opencode",
        }
    }

    /// The human-facing name for report copy. Reports name the harness whose
    /// rows they render, so opencode output never claims to be Claude Code's.
    pub fn display_name(&self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Opencode => "opencode",
        }
    }

    /// The store file name under the user state dir. Claude keeps the
    /// historical name; every later harness gets its own file, so an old
    /// store is never reinterpreted as another harness's rows.
    pub fn db_file_name(&self) -> &'static str {
        match self {
            Harness::Claude => "cclens.db",
            Harness::Opencode => "cclens-opencode.db",
        }
    }

    /// The opencode database: `$XDG_DATA_HOME/opencode/opencode.db`, else
    /// `~/.local/share/opencode/opencode.db`. A non-absolute data home is
    /// invalid per the XDG spec and ignored, mirroring `store_path`.
    pub fn opencode_db_path(data_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
        if let Some(dir) = data_home.filter(|dir| dir.is_absolute()) {
            return Some(dir.join("opencode").join("opencode.db"));
        }
        Some(
            home.filter(|dir| dir.is_absolute())?
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db"),
        )
    }

    /// The opencode config root: `$XDG_CONFIG_HOME/opencode`, else
    /// `~/.config/opencode`. Same absolute-path rule as the database.
    pub fn opencode_config_dir(config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
        if let Some(dir) = config_home.filter(|dir| dir.is_absolute()) {
            return Some(dir.join("opencode"));
        }
        Some(
            home.filter(|dir| dir.is_absolute())?
                .join(".config")
                .join("opencode"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_harnesses_and_rejects_the_rest() {
        assert_eq!(Harness::parse("claude"), Some(Harness::Claude));
        assert_eq!(Harness::parse("opencode"), Some(Harness::Opencode));
        assert_eq!(Harness::parse("auto"), None);
        assert_eq!(Harness::parse(""), None);
    }

    #[test]
    fn labels_round_trip_through_parse() {
        for harness in [Harness::Claude, Harness::Opencode] {
            assert_eq!(Harness::parse(harness.label()), Some(harness));
        }
    }

    #[test]
    fn display_names_name_the_harness() {
        assert_eq!(Harness::Claude.display_name(), "Claude Code");
        assert_eq!(Harness::Opencode.display_name(), "opencode");
    }

    #[test]
    fn claude_keeps_the_historical_db_name() {
        assert_eq!(Harness::Claude.db_file_name(), "cclens.db");
        assert_eq!(Harness::Opencode.db_file_name(), "cclens-opencode.db");
    }

    #[test]
    fn opencode_db_prefers_an_absolute_data_home() {
        assert_eq!(
            Harness::opencode_db_path(
                Some(Path::new("/tmp/example/data")),
                Some(Path::new("/tmp/example/home"))
            ),
            Some(PathBuf::from("/tmp/example/data/opencode/opencode.db"))
        );
    }

    #[test]
    fn opencode_db_falls_back_to_the_home_share_dir() {
        assert_eq!(
            Harness::opencode_db_path(None, Some(Path::new("/tmp/example/home"))),
            Some(PathBuf::from(
                "/tmp/example/home/.local/share/opencode/opencode.db"
            ))
        );
    }

    #[test]
    fn opencode_db_ignores_a_relative_data_home() {
        assert_eq!(
            Harness::opencode_db_path(
                Some(Path::new("relative/data")),
                Some(Path::new("/tmp/example/home"))
            ),
            Some(PathBuf::from(
                "/tmp/example/home/.local/share/opencode/opencode.db"
            ))
        );
    }

    #[test]
    fn opencode_db_without_a_home_has_no_answer() {
        assert_eq!(Harness::opencode_db_path(None, None), None);
    }

    #[test]
    fn opencode_config_prefers_an_absolute_config_home() {
        assert_eq!(
            Harness::opencode_config_dir(
                Some(Path::new("/tmp/example/config")),
                Some(Path::new("/tmp/example/home"))
            ),
            Some(PathBuf::from("/tmp/example/config/opencode"))
        );
    }

    #[test]
    fn opencode_config_falls_back_to_home_dot_config() {
        assert_eq!(
            Harness::opencode_config_dir(None, Some(Path::new("/tmp/example/home"))),
            Some(PathBuf::from("/tmp/example/home/.config/opencode"))
        );
        assert_eq!(Harness::opencode_config_dir(None, None), None);
    }
}
