//! XDG Base Directory resolution (#128), shared by every tool: one place
//! that reads `$XDG_CONFIG_HOME`, `$XDG_CACHE_HOME` and `$XDG_STATE_HOME`.
//! The spec's fallbacks (`~/.config`, `~/.cache`, `~/.local/state`) apply
//! on every platform, macOS included, where the `dirs` crate would pick
//! `~/Library` or nothing at all.

use std::ffi::OsString;
use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/<app>`, else `~/.config/<app>`; `None` without `$HOME`.
pub fn config_dir(app: &str) -> Option<PathBuf> {
    from_env("XDG_CONFIG_HOME", ".config", app)
}

/// `$XDG_CACHE_HOME/<app>`, else `~/.cache/<app>`; `None` without `$HOME`.
pub fn cache_dir(app: &str) -> Option<PathBuf> {
    from_env("XDG_CACHE_HOME", ".cache", app)
}

/// `$XDG_STATE_HOME/<app>`, else `~/.local/state/<app>`; `None` without
/// `$HOME`.
pub fn state_dir(app: &str) -> Option<PathBuf> {
    from_env("XDG_STATE_HOME", ".local/state", app)
}

fn from_env(var: &str, default: &str, app: &str) -> Option<PathBuf> {
    resolve(
        std::env::var_os(var),
        std::env::var_os("HOME"),
        default,
        app,
    )
}

/// The pure resolution behind [`config_dir`], [`cache_dir`] and
/// [`state_dir`]: `xdg` (the variable's value) when set and non-empty,
/// else `home` joined with `default`, then `app`. Takes the environment as
/// arguments so tests need not mutate the process's.
pub fn resolve(
    xdg: Option<OsString>,
    home: Option<OsString>,
    default: &str,
    app: &str,
) -> Option<PathBuf> {
    let base = match xdg.filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home.filter(|h| !h.is_empty())?).join(default),
    };
    Some(base.join(app))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn the_variable_wins_when_set() {
        let dir = resolve(
            os("/custom/state"),
            os("/home/me"),
            ".local/state",
            "column-rs",
        );
        assert_eq!(dir.unwrap(), PathBuf::from("/custom/state/column-rs"));
    }

    #[test]
    fn falls_back_to_home_when_the_variable_is_unset() {
        let dir = resolve(None, os("/home/me"), ".local/state", "column-rs");
        assert_eq!(
            dir.unwrap(),
            PathBuf::from("/home/me/.local/state/column-rs")
        );
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let dir = resolve(os(""), os("/home/me"), ".cache", "trigrep");
        assert_eq!(dir.unwrap(), PathBuf::from("/home/me/.cache/trigrep"));
    }

    #[test]
    fn none_without_the_variable_or_home() {
        assert_eq!(resolve(None, None, ".config", "loglume"), None);
        assert_eq!(resolve(os(""), os(""), ".config", "loglume"), None);
    }

    #[test]
    fn the_variable_needs_no_home() {
        let dir = resolve(os("/xdg/config"), None, ".config", "db-studio");
        assert_eq!(dir.unwrap(), PathBuf::from("/xdg/config/db-studio"));
    }
}
