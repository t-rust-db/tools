//! Command history storage, navigation, and file persistence.

use std::fs;
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

/// Default history file path for the given app name, e.g. `history_path("column-rs")`
/// resolves to `~/.local/state/column-rs_history` (or the platform equivalent).
pub fn history_path(app_name: &str) -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::home_dir)
        .map(|d| d.join(format!(".{app_name}_history")))
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
}
