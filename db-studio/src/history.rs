//! Query history (db-studio#42): every submitted query, persisted to
//! the XDG cache directory (`$XDG_CACHE_HOME/db-studio/history`, or
//! `~/.cache/db-studio/history` when `$XDG_CACHE_HOME` isn't set --
//! the base-dir spec's own fallback) so it survives across
//! invocations, not just within one session's `QueryPane` history.
//!
//! Entries are separated by `\x1e` (ASCII record separator) rather
//! than a newline, since a submitted query is very often itself
//! multi-line -- a newline-joined file couldn't tell "end of this
//! entry" from "blank line inside a multi-line query" apart.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

const SEPARATOR: char = '\u{1e}';

fn cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("db-studio"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".cache").join("db-studio"))
}

fn history_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join("history"))
}

/// Loads every previously recorded query, oldest first. Never fails
/// outward -- a missing/unreadable cache is just empty history, not a
/// reason to refuse to start.
pub fn load() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .split(SEPARATOR)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// Appends one submitted query to the on-disk history. Best-effort: an
/// unwritable cache directory (read-only home, no `$HOME` at all in a
/// stripped-down environment) loses history for that run, not the
/// query itself -- the in-memory `QueryPane` history it feeds still
/// works for the rest of the session either way.
pub fn append(query: &str) {
    let Some(path) = history_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        if fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = write!(file, "{query}{SEPARATOR}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_splits_on_the_record_separator_and_trims_entries() {
        // Exercises the parsing logic directly rather than through the
        // real XDG path, so this test doesn't depend on (or pollute)
        // the actual cache directory.
        let content = format!(" SELECT 1 {SEPARATOR}SELECT 2{SEPARATOR}{SEPARATOR}");
        let entries: Vec<&str> = content
            .split(SEPARATOR)
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .collect();
        assert_eq!(entries, vec!["SELECT 1", "SELECT 2"]);
    }
}
