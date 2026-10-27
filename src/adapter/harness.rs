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
    Kilocode,
}

impl Harness {
    /// Parse `--harness`. `None` for anything else, so the caller can fail
    /// with the valid list.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Harness::Claude),
            "opencode" => Some(Harness::Opencode),
            "kilocode" => Some(Harness::Kilocode),
            _ => None,
        }
    }

    /// The `meta.harness` value and the error-hint vocabulary.
    pub fn label(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Opencode => "opencode",
            Harness::Kilocode => "kilocode",
        }
    }

    /// The human-facing name for report copy. Reports name the harness whose
    /// rows they render, so opencode output never claims to be Claude Code's.
    pub fn display_name(&self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Opencode => "opencode",
            Harness::Kilocode => "kilocode",
        }
    }

    /// The indefinite article for the display name in prose ("a Claude Code
    /// advisor", "an opencode advisor").
    pub fn article(&self) -> &'static str {
        match self {
            Harness::Claude => "a",
            Harness::Opencode => "an",
            Harness::Kilocode => "a",
        }
    }

    /// The instruction file reports point at: the file a file-map or rule
    /// fix lands in. kilocode reads the shared `AGENTS.md` convention.
    pub fn instruction_file(&self) -> &'static str {
        match self {
            Harness::Claude => "CLAUDE.md",
            Harness::Opencode => "AGENTS.md",
            Harness::Kilocode => "AGENTS.md",
        }
    }

    /// How reports name the project config layer that owns a fix. kilocode's
    /// project-level conventions (`.kilocode/rules` and the like) are not
    /// observed, so the copy names the one instruction file that exists.
    pub fn project_config_ref(&self) -> &'static str {
        match self {
            Harness::Claude => ".claude / CLAUDE.md",
            Harness::Opencode => ".opencode / AGENTS.md",
            Harness::Kilocode => "AGENTS.md",
        }
    }

    /// How reports name the global config layer that owns a fix.
    pub fn global_config_ref(&self) -> &'static str {
        match self {
            Harness::Claude => "~/.claude",
            Harness::Opencode => "~/.config/opencode",
            Harness::Kilocode => "~/.config/kilo",
        }
    }

    /// The noun for analyzed input in prose ("their Claude Code session
    /// transcripts", "their opencode sessions").
    pub fn source_noun(&self) -> &'static str {
        match self {
            Harness::Claude => "session transcripts",
            Harness::Opencode => "sessions",
            Harness::Kilocode => "sessions",
        }
    }

    /// The raw input advisors must not re-parse ("the raw ~/.claude/projects
    /// transcripts", "the raw opencode database").
    pub fn raw_source(&self) -> &'static str {
        match self {
            Harness::Claude => "~/.claude/projects transcripts",
            Harness::Opencode => "opencode database",
            Harness::Kilocode => "kilocode task files",
        }
    }

    /// The config files an advisor opens to pin down a root cause.
    pub fn config_inventory(&self) -> &'static str {
        match self {
            Harness::Claude => "CLAUDE.md, rules, hooks, skills, and settings",
            Harness::Opencode => "AGENTS.md, commands, agents, and opencode.json",
            Harness::Kilocode => "AGENTS.md, commands, skills, and kilo.json",
        }
    }

    /// The readable-config breakdown in the `overhead` view.
    pub fn readable_config(&self) -> &'static str {
        match self {
            Harness::Claude => "CLAUDE.md, always-on rules, skill/agent descriptions",
            Harness::Opencode => "AGENTS.md, commands, agent descriptions",
            Harness::Kilocode => "AGENTS.md, command and skill descriptions",
        }
    }

    /// The unit a refresh re-analyzes ("transcript(s)", "session(s)").
    pub fn input_unit(&self) -> &'static str {
        match self {
            Harness::Claude => "transcript(s)",
            Harness::Opencode => "session(s)",
            Harness::Kilocode => "session(s)",
        }
    }

    /// The `inventory` framing block. Kind lists differ per harness, so the
    /// whole block is per-harness rather than interpolated.
    pub fn inventory_preamble(&self) -> &'static str {
        match self {
            Harness::Claude => {
                "Everything installed in your config — skills, rules, agents, MCP servers,\n\
             CLAUDE.md — with where it is installed (scope) and whether it was ever used.\n\
             \"start tok\" is what every session pays for having it installed; \"static tok\"\n\
             is the whole definition, which for a skill or agent is paid only on invocation.\n\
             UNUSED marks delete candidates; rules and CLAUDE.md are catalog-only because\n\
             using them leaves no trace in transcripts."
            }
            Harness::Opencode => {
                "Everything installed in your config — commands, agents,\n\
             AGENTS.md — with where it is installed (scope) and whether it was ever used.\n\
             \"start tok\" is what every session pays for having it installed; \"static tok\"\n\
             is the whole definition, which for a command or agent is paid only on invocation.\n\
             UNUSED marks delete candidates; commands and AGENTS.md are catalog-only because\n\
             using them leaves no trace in sessions."
            }
            Harness::Kilocode => {
                "Everything installed in your config — AGENTS.md, .kilorules, commands,\n\
             agents, skills — is listed with where it is installed (scope) and whether it\n\
             was ever used. UNUSED marks delete candidates; commands are catalog-only\n\
             because using them leaves no trace in sessions."
            }
        }
    }

    /// The store file name under the user state dir. Claude keeps the
    /// historical name; every later harness gets its own file, so an old
    /// store is never reinterpreted as another harness's rows.
    pub fn db_file_name(&self) -> &'static str {
        match self {
            Harness::Claude => "agentlens.db",
            Harness::Opencode => "agentlens-opencode.db",
            Harness::Kilocode => "agentlens-kilocode.db",
        }
    }

    /// The editor globalStorage task directories kilocode writes, in the
    /// order the default resolution probes them. The path varies with the
    /// editor install (Code / Cursor / VSCodium) and the platform, so the
    /// list — not one hardcoded path — is the contract; `--projects` always
    /// wins over it.
    pub fn kilocode_tasks_candidates(home: Option<&Path>) -> Vec<PathBuf> {
        let Some(home) = home.filter(|dir| dir.is_absolute()) else {
            return Vec::new();
        };
        let editors: [&str; 3] = if cfg!(target_os = "macos") {
            [
                "Library/Application Support/Code/User/globalStorage",
                "Library/Application Support/Cursor/User/globalStorage",
                "Library/Application Support/VSCodium/User/globalStorage",
            ]
        } else {
            [
                ".config/Code/User/globalStorage",
                ".config/Cursor/User/globalStorage",
                ".config/VSCodium/User/globalStorage",
            ]
        };
        editors
            .iter()
            .map(|editor| home.join(editor).join("kilocode.kilo-code").join("tasks"))
            .collect()
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

    /// The global kilo config root: `$XDG_CONFIG_HOME/kilo` when that variable
    /// names an absolute path, else `~/.config/kilo` — the XDG rule, mirroring
    /// how the store resolution treats `XDG_STATE_HOME`.
    pub fn kilocode_config_dir(config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
        if let Some(dir) = config_home.filter(|dir| dir.is_absolute()) {
            return Some(dir.join("kilo"));
        }
        Some(
            home.filter(|dir| dir.is_absolute())?
                .join(".config")
                .join("kilo"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_three_harnesses_and_rejects_the_rest() {
        assert_eq!(Harness::parse("claude"), Some(Harness::Claude));
        assert_eq!(Harness::parse("opencode"), Some(Harness::Opencode));
        assert_eq!(Harness::parse("kilocode"), Some(Harness::Kilocode));
        assert_eq!(Harness::parse("auto"), None);
        assert_eq!(Harness::parse(""), None);
    }

    #[test]
    fn labels_round_trip_through_parse() {
        for harness in [Harness::Claude, Harness::Opencode, Harness::Kilocode] {
            assert_eq!(Harness::parse(harness.label()), Some(harness));
        }
    }

    #[test]
    fn display_names_name_the_harness() {
        assert_eq!(Harness::Claude.display_name(), "Claude Code");
        assert_eq!(Harness::Opencode.display_name(), "opencode");
        assert_eq!(Harness::Kilocode.display_name(), "kilocode");
        assert_eq!(Harness::Claude.article(), "a");
        assert_eq!(Harness::Opencode.article(), "an");
        assert_eq!(Harness::Kilocode.article(), "a");
    }

    #[test]
    fn report_copy_names_each_harness_own_config() {
        assert_eq!(Harness::Claude.instruction_file(), "CLAUDE.md");
        assert_eq!(Harness::Opencode.instruction_file(), "AGENTS.md");
        assert_eq!(Harness::Claude.project_config_ref(), ".claude / CLAUDE.md");
        assert_eq!(
            Harness::Opencode.project_config_ref(),
            ".opencode / AGENTS.md"
        );
        assert_eq!(Harness::Claude.global_config_ref(), "~/.claude");
        assert_eq!(Harness::Opencode.global_config_ref(), "~/.config/opencode");
        assert_eq!(Harness::Claude.source_noun(), "session transcripts");
        assert_eq!(Harness::Opencode.source_noun(), "sessions");
        assert_eq!(
            Harness::Claude.raw_source(),
            "~/.claude/projects transcripts"
        );
        assert_eq!(Harness::Opencode.raw_source(), "opencode database");
        assert_eq!(Harness::Claude.input_unit(), "transcript(s)");
        assert_eq!(Harness::Opencode.input_unit(), "session(s)");
        assert!(Harness::Opencode.inventory_preamble().contains("AGENTS.md"));
        assert!(!Harness::Opencode.inventory_preamble().contains("CLAUDE.md"));
    }

    #[test]
    fn claude_keeps_the_historical_db_name() {
        assert_eq!(Harness::Claude.db_file_name(), "agentlens.db");
        assert_eq!(Harness::Opencode.db_file_name(), "agentlens-opencode.db");
        assert_eq!(Harness::Kilocode.db_file_name(), "agentlens-kilocode.db");
    }

    #[test]
    fn kilocode_tasks_candidates_cover_the_known_editors() {
        // The tasks dir varies with the editor install (Code / Cursor /
        // VSCodium) and the platform, so the list is what the shell probes —
        // in a fixed order, `--projects` always winning over it.
        let candidates = Harness::kilocode_tasks_candidates(Some(Path::new("/tmp/example/home")));
        assert_eq!(candidates.len(), 3);
        for candidate in &candidates {
            assert!(
                candidate
                    .to_string_lossy()
                    .ends_with("kilocode.kilo-code/tasks")
            );
            assert!(candidate.starts_with("/tmp/example/home"));
        }
    }

    #[test]
    fn kilocode_tasks_candidates_without_a_home_are_empty() {
        assert!(Harness::kilocode_tasks_candidates(None).is_empty());
        assert!(Harness::kilocode_tasks_candidates(Some(Path::new("relative/home"))).is_empty());
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
