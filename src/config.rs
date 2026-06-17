//! User configuration loaded from a `.hotcarpet.toml` file.
//!
//! Today the only thing it controls is how file extensions map to the built-in
//! language analyzers: an analyzer's default extension list can be replaced
//! wholesale (`extensions`) and/or extended (`extra_extensions`). See
//! [`crate::analyzer::AnalyzerRegistry::apply_config`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// File name searched for when no explicit `--config` is given.
pub const CONFIG_FILENAME: &str = ".hotcarpet.toml";

/// The whole config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Per-analyzer overrides, keyed by analyzer name (case-insensitive),
    /// e.g. `[analyzers.typescript]`.
    #[serde(default)]
    pub analyzers: HashMap<String, AnalyzerConfig>,
}

/// Overrides for one language analyzer.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzerConfig {
    /// Replace the analyzer's built-in extension list entirely. When omitted,
    /// the built-in list is kept.
    pub extensions: Option<Vec<String>>,
    /// Extensions to add on top of the (built-in or replaced) list.
    #[serde(default)]
    pub extra_extensions: Vec<String>,
}

impl Config {
    /// Resolve the config for a run. With an explicit `path`, that file must
    /// exist and parse. Otherwise search upward from `start` for
    /// [`CONFIG_FILENAME`]; finding none yields the default (empty) config.
    pub fn resolve(path: Option<&str>, start: &str) -> Result<Config> {
        match path {
            Some(p) => Self::load(Path::new(p)),
            None => match find_upward(start) {
                Some(found) => Self::load(&found),
                None => Ok(Config::default()),
            },
        }
    }

    fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file '{}'", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("failed to parse config file '{}'", path.display()))
    }
}

/// Walk up from `start` looking for a `.hotcarpet.toml`, returning the first hit.
fn find_upward(start: &str) -> Option<PathBuf> {
    let start = Path::new(start);
    // Canonicalize so a relative `.` resolves to an absolute path and `pop()`
    // can climb to the filesystem root; fall back to the raw path if it can't
    // be canonicalized (e.g. it does not exist yet).
    let mut dir = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extension_overrides() {
        let config: Config = toml::from_str(
            r#"
            [analyzers.typescript]
            extensions = ["ts", "tsx"]
            extra_extensions = ["vue"]
            "#,
        )
        .unwrap();

        let ts = &config.analyzers["typescript"];
        assert_eq!(
            ts.extensions.as_deref(),
            Some(&["ts".to_string(), "tsx".to_string()][..])
        );
        assert_eq!(ts.extra_extensions, vec!["vue".to_string()]);
    }

    #[test]
    fn extra_extensions_default_to_empty() {
        let config: Config = toml::from_str(
            r#"
            [analyzers.typescript]
            extra_extensions = ["vue"]
            "#,
        )
        .unwrap();

        let ts = &config.analyzers["typescript"];
        assert_eq!(ts.extensions, None);
        assert_eq!(ts.extra_extensions, vec!["vue".to_string()]);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = toml::from_str::<Config>(
            r#"
            [analyzers.typescript]
            extension = ["ts"]
            "#,
        );
        assert!(err.is_err(), "a typo'd key should be rejected");
    }

    #[test]
    fn empty_config_is_default() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.analyzers.is_empty());
    }
}
