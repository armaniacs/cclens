//! The tool's own config file: `$XDG_CONFIG_HOME/saniwalens/config.toml`
//! (or `~/.config/saniwalens/config.toml`).
//!
//! The file is optional — without it every invocation behaves exactly as
//! before. When present it supplies defaults the flags override: a default
//! harness, a default store, and named profiles `--profile` selects.
//! Resolution is pure over the loaded file so it is unit-testable; reading
//! the file from disk is a thin shell around it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The whole file. Every field is optional.
#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    /// Used when neither `--harness` nor a profile supplies one.
    #[serde(default)]
    pub default_harness: Option<String>,
    /// Used when neither `--db` nor a profile supplies one. A relative path
    /// resolves against the directory holding the config file, never the
    /// current working directory.
    #[serde(default)]
    pub db: Option<PathBuf>,
    /// Named presets `--profile` selects.
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
}

/// One named profile. Every field is optional; an absent field falls through
/// to the top-level default, then to the builtin.
#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct ProfileConfig {
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub db: Option<PathBuf>,
}

/// What the flags plus the file resolve to. `harness` stays the raw string so
/// the caller feeds it through the same parse — and the same error wording —
/// as an explicit `--harness`. `db` is absolute by the time it leaves here.
#[derive(Debug, Default, PartialEq)]
pub struct ResolvedConfig {
    pub harness: Option<String>,
    pub db: Option<PathBuf>,
}

/// Merge one invocation's inputs, strongest first: explicit flags, the named
/// profile, the file's top-level defaults, then nothing (the caller falls
/// back to the builtins). A relative `db` joins the directory holding the
/// config file, never the current working directory.
pub fn resolve(
    cli_harness: Option<&str>,
    cli_profile: Option<&str>,
    cli_db: Option<&Path>,
    file: Option<&FileConfig>,
    config_dir: Option<&Path>,
) -> Result<ResolvedConfig> {
    let profile = match (cli_profile, file) {
        (Some(name), Some(file)) => Some(
            file.profiles
                .get(name)
                .with_context(|| unknown_profile(name, &file.profiles))?,
        ),
        (Some(name), None) => anyhow::bail!("unknown --profile '{name}' (no config file)"),
        _ => None,
    };
    let harness = cli_harness
        .map(str::to_string)
        .or_else(|| profile.and_then(|p| p.harness.clone()))
        .or_else(|| file.and_then(|f| f.default_harness.clone()));
    let db = match (
        cli_db,
        profile.and_then(|p| p.db.as_deref()),
        file.and_then(|f| f.db.as_deref()),
    ) {
        // An explicit `--db` is taken as is, even when relative.
        (Some(p), _, _) => Some(p.to_path_buf()),
        // The file's own entries rebase onto the config directory. The None
        // dir arm is unreachable in practice (a file entry implies a file was
        // read, which implies its directory); it keeps the path untouched
        // rather than inventing a base.
        (None, Some(p), _) | (None, None, Some(p)) => Some(match (p.is_absolute(), config_dir) {
            (true, _) => p.to_path_buf(),
            (false, Some(dir)) => dir.join(p),
            (false, None) => p.to_path_buf(),
        }),
        (None, None, None) => None,
    };
    Ok(ResolvedConfig { harness, db })
}

fn unknown_profile(name: &str, profiles: &HashMap<String, ProfileConfig>) -> String {
    let mut names: Vec<&str> = profiles.keys().map(String::as_str).collect();
    names.sort_unstable();
    if names.is_empty() {
        format!("unknown --profile '{name}' (no profiles defined)")
    } else {
        format!(
            "unknown --profile '{name}' (available: {})",
            names.join(", ")
        )
    }
}

fn parse(content: &str, source: &str) -> Result<FileConfig> {
    toml::from_str(content).with_context(|| format!("parse {source}"))
}

/// Read the tool's own config file. A missing file is not an error — it means
/// "behave exactly as before" — but anything else wrong with it is.
pub fn load(path: &Path) -> Result<Option<FileConfig>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("read {}", path.display()));
        }
    };
    parse(&content, &path.display().to_string()).map(Some)
}

#[cfg(test)]
fn load_str(content: &str) -> Result<FileConfig> {
    parse(content, "test")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_dir() -> PathBuf {
        PathBuf::from("/tmp/example/config/saniwalens")
    }

    fn file_with_profile() -> FileConfig {
        toml::from_str(
            r#"
default_harness = "kilocode"

[profiles.work]
harness = "claude"
db = "work.db"
"#,
        )
        .unwrap()
    }

    #[test]
    fn default_harness_applies_without_flags() {
        let resolved = resolve(
            None,
            None,
            None,
            Some(&file_with_profile()),
            Some(&example_dir()),
        )
        .unwrap();
        assert_eq!(resolved.harness.as_deref(), Some("kilocode"));
    }

    #[test]
    fn profile_selects_harness_and_config_relative_db() {
        let resolved = resolve(
            None,
            Some("work"),
            None,
            Some(&file_with_profile()),
            Some(&example_dir()),
        )
        .unwrap();
        assert_eq!(resolved.harness.as_deref(), Some("claude"));
        assert_eq!(resolved.db, Some(example_dir().join("work.db")));
    }

    #[test]
    fn explicit_flags_beat_everything() {
        let resolved = resolve(
            Some("opencode"),
            Some("work"),
            Some(Path::new("/tmp/explicit.db")),
            Some(&file_with_profile()),
            Some(&example_dir()),
        )
        .unwrap();
        assert_eq!(resolved.harness.as_deref(), Some("opencode"));
        assert_eq!(resolved.db, Some(PathBuf::from("/tmp/explicit.db")));
    }

    #[test]
    fn absolute_db_in_file_is_taken_as_is() {
        let file: FileConfig = toml::from_str(r#"db = "/tmp/abs.db""#).unwrap();
        let resolved = resolve(None, None, None, Some(&file), Some(&example_dir())).unwrap();
        assert_eq!(resolved.db, Some(PathBuf::from("/tmp/abs.db")));
    }

    #[test]
    fn explicit_relative_db_stays_cwd_relative() {
        // An explicit `--db` is taken as is, even when relative — only the
        // file's own entries rebase onto the config directory.
        let resolved = resolve(
            None,
            Some("work"),
            Some(Path::new("relative.db")),
            Some(&file_with_profile()),
            Some(&example_dir()),
        )
        .unwrap();
        assert_eq!(resolved.db, Some(PathBuf::from("relative.db")));
    }

    #[test]
    fn profile_without_any_file_is_an_error() {
        let err = resolve(None, Some("work"), None, None, Some(&example_dir())).unwrap_err();
        assert!(format!("{err:#}").contains("no config file"));
    }

    #[test]
    fn unknown_profile_names_the_available_ones() {
        let err = resolve(
            None,
            Some("play"),
            None,
            Some(&file_with_profile()),
            Some(&example_dir()),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("play"), "{msg}");
        assert!(msg.contains("work"), "{msg}");
    }

    #[test]
    fn broken_toml_is_an_error_not_a_fallback() {
        assert!(load_str("default_harness = = ==").is_err());
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compatibility() {
        let file =
            load_str("future_key = true\n[profiles.work]\nharness = \"claude\"\nnew_thing = 1")
                .unwrap();
        assert_eq!(file.default_harness, None);
        assert_eq!(
            file.profiles["work"],
            ProfileConfig {
                harness: Some("claude".to_string()),
                db: None,
            }
        );
    }

    #[test]
    fn no_file_means_no_resolution() {
        let resolved = resolve(None, None, None, None, Some(&example_dir())).unwrap();
        assert_eq!(resolved.harness, None);
        assert_eq!(resolved.db, None);
    }
}
