//! XDG-based configuration (`config.toml`): theme color overrides
//! (db-studio#61), ported near-verbatim from `t-rust-db/loglume#11`'s
//! own config module -- same XDG resolution (db-cli's since tools#128),
//! same load/save/round-trip-through-toml
//! shape, same "`db-studio config`" print-the-resolved-config
//! convention as `loglume config`.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub theme: ThemeConfig,
}

/// Per-key hex overrides for `theme.rs`'s catppuccin-mocha defaults
/// (db-studio#61). Every field mirrors one of `theme.rs`'s existing
/// color functions by name; an unset (`None`) key keeps rendering
/// exactly as it does today -- these are overrides on top of the
/// default palette, not a replacement for it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub subtext: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub string_literal: Option<String>,
    #[serde(default)]
    pub number_literal: Option<String>,
    #[serde(default)]
    pub identifier: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub selection_bg: Option<String>,
}

impl Config {
    /// Load the config from its resolved path, or defaults if absent.
    pub fn load() -> io::Result<Self> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
}

/// `$XDG_CONFIG_HOME/db-studio/config.toml`, falling back to
/// `$HOME/.config/db-studio/config.toml` per the XDG Base Directory
/// Spec (used verbatim, regardless of platform).
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// db-cli's XDG resolution (tools#128); `./.config/db-studio` without
/// `$HOME`.
fn config_dir() -> PathBuf {
    db_cli::xdg::config_dir("db-studio").unwrap_or_else(|| PathBuf::from(".config/db-studio"))
}

/// `db-studio config`: print the resolved config path and its contents.
pub fn print_resolved() -> io::Result<()> {
    let path = config_path();
    let cfg = Config::load()?;
    let text = toml::to_string_pretty(&cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    println!("# {}", path.display());
    print!("{text}");
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_theme_overrides() {
        let cfg = Config::default();
        assert!(cfg.theme.text.is_none());
        assert!(cfg.theme.accent.is_none());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.theme.accent = Some("#ff00ff".to_string());

        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(parsed.theme.accent, Some("#ff00ff".to_string()));
        assert!(parsed.theme.text.is_none());
    }

    #[test]
    fn missing_config_file_loads_as_default() {
        // toml::from_str("") parses to an empty document, which with
        // #[serde(default)] on every field is equivalent to Config::default().
        let parsed: Config = toml::from_str("").unwrap();
        assert!(parsed.theme.text.is_none());
    }

    #[test]
    fn empty_theme_section_loads_as_default() {
        let parsed: Config = toml::from_str("[theme]\n").unwrap();
        assert!(parsed.theme.text.is_none());
        assert!(parsed.theme.selection_bg.is_none());
    }
}
