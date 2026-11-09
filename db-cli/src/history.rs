//! Command history storage, navigation, and file persistence.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_HISTORY: usize = 1000;

/// In-memory command history, capped at the most recent 1000 entries,
/// with plain-text file persistence (one entry per line).
pub struct History {
    entries: Vec<String>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// An empty history.
    pub fn new() -> Self {
        History {
            entries: Vec::new(),
        }
    }

    /// Appends `line` (trimmed); blank lines and repeats of the last entry
    /// are ignored, and the oldest entry is dropped past the cap.
    pub fn add(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // Don't duplicate consecutive entries.
        if self.entries.last().map(|s| s.as_str()) == Some(line) {
            return;
        }
        self.entries.push(line.to_string());
        if self.entries.len() > MAX_HISTORY {
            self.entries.remove(0);
        }
    }

    /// Appends the lines of `path`; a missing or unreadable file is a no-op.
    pub fn load(&mut self, path: &Path) {
        if let Ok(contents) = fs::read_to_string(path) {
            for line in contents.lines() {
                if !line.is_empty() {
                    self.entries.push(line.to_string());
                }
            }
            if self.entries.len() > MAX_HISTORY {
                self.entries = self.entries.split_off(self.entries.len() - MAX_HISTORY);
            }
        }
    }

    /// Writes all entries to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let contents = self.entries.join("\n");
        fs::write(path, contents).ok();
    }

    /// Navigate to the previous (older) entry.
    pub fn prev(&self, idx: &mut Option<usize>) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let new_idx = match *idx {
            None => self.entries.len().saturating_sub(1),
            Some(i) => i.saturating_sub(1),
        };
        *idx = Some(new_idx);
        self.entries.get(new_idx).map(|s| s.as_str())
    }

    /// Navigate to the next (newer) entry.
    pub fn next(&self, idx: &mut Option<usize>) -> Option<&str> {
        let i = (*idx)?;
        let new_idx = i + 1;
        if new_idx >= self.entries.len() {
            *idx = None;
            return None;
        }
        *idx = Some(new_idx);
        self.entries.get(new_idx).map(|s| s.as_str())
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The history file for `app_name`: `$XDG_STATE_HOME/<app>/history`, else
/// `~/.local/state/<app>/history` (#128); `None` without either variable.
/// A history at the old `~/.<app>_history` is moved in once: appended to
/// the new file, then removed.
pub fn history_path(app_name: &str) -> Option<PathBuf> {
    let path = crate::xdg::state_dir(app_name)?.join("history");
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        merge_legacy(
            &PathBuf::from(home).join(format!(".{app_name}_history")),
            &path,
        );
    }
    Some(path)
}

/// Appends `old`'s lines to `new` and removes `old`; leaves `old` alone
/// if anything fails, so no history is lost.
fn merge_legacy(old: &Path, new: &Path) {
    let Ok(contents) = fs::read_to_string(old) else {
        return;
    };
    if let Some(parent) = new.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let appended = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(new)
        // `save` writes no trailing newline; a blank line is skipped on load.
        .and_then(|mut f| f.write_all(format!("\n{contents}").as_bytes()));
    if appended.is_ok() {
        fs::remove_file(old).ok();
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn add_skips_blank_and_consecutive_duplicates() {
        let mut h = History::new();
        h.add("select 1");
        h.add("  ");
        h.add("select 1");
        h.add("select 2");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn prev_next_navigate_in_order() {
        let mut h = History::new();
        h.add("a");
        h.add("b");
        h.add("c");
        let mut idx = None;
        assert_eq!(h.prev(&mut idx), Some("c"));
        assert_eq!(h.prev(&mut idx), Some("b"));
        assert_eq!(h.next(&mut idx), Some("c"));
        assert_eq!(h.next(&mut idx), None);
    }

    #[test]
    fn load_and_save_round_trip() {
        let dir = std::env::temp_dir().join(format!("db-cli-test-{}", std::process::id()));
        let path = dir.join("hist");
        let mut h = History::new();
        h.add("select 1");
        h.add("select 2");
        h.save(&path);

        let mut loaded = History::new();
        loaded.load(&path);
        assert_eq!(loaded.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("db-cli-merge-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn a_legacy_history_moves_into_a_missing_one() {
        let dir = scratch("missing");
        let (old, new) = (dir.join(".app_history"), dir.join("state/app/history"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(&old, "select 1\nselect 2").unwrap();
        merge_legacy(&old, &new);
        assert!(!old.exists());
        let mut h = History::new();
        h.load(&new);
        assert_eq!(h.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_legacy_history_is_appended_after_the_existing_one() {
        let dir = scratch("both");
        let (old, new) = (dir.join(".app_history"), dir.join("state/app/history"));
        let mut existing = History::new();
        existing.add("select 'older'");
        existing.save(&new);
        fs::write(&old, "select 'newer'").unwrap();
        merge_legacy(&old, &new);
        assert!(!old.exists());
        let mut h = History::new();
        h.load(&new);
        let mut idx = None;
        assert_eq!(h.prev(&mut idx), Some("select 'newer'"));
        assert_eq!(h.prev(&mut idx), Some("select 'older'"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_legacy_history_leaves_the_new_one_alone() {
        let dir = scratch("none");
        let new = dir.join("state/app/history");
        merge_legacy(&dir.join(".app_history"), &new);
        assert!(!new.exists());
    }
}
