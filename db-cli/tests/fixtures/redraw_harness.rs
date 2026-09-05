//! Minimal `read_line` loop driven over a pty by `tests/redraw_pty.rs`
//! (issue #1) to exercise the wrapped-line redraw path against a real
//! (virtual) terminal, not just `layout`'s pure math in isolation.
//!
//! Also optionally installs a test [`Completer`]/[`Highlighter`] (#6),
//! gated behind env vars so the default-behavior tests (issue #1/#2)
//! keep exercising `Readline::new()`'s untouched defaults.

use db_cli::editor::Readline;
use db_cli::{Completer, Highlighter};

/// A fixed three-keyword completer for testing Tab -- not meant to be
/// realistic, just deterministic: completes an alphanumeric/`_` prefix
/// against `SELECT`/`FROM`/`WHERE`.
struct TestCompleter;

impl Completer for TestCompleter {
    fn complete(&self, line: &str, cursor: usize) -> (usize, Vec<String>) {
        const KEYWORDS: &[&str] = &["SELECT", "SELF", "FROM", "WHERE"];
        let chars: Vec<char> = line.chars().collect();
        let mut start = cursor;
        while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
            start -= 1;
        }
        let prefix: String = chars[start..cursor].iter().collect();
        if prefix.is_empty() {
            return (start, Vec::new());
        }
        let prefix_upper = prefix.to_ascii_uppercase();
        let candidates: Vec<String> = KEYWORDS
            .iter()
            .filter(|k| k.starts_with(&prefix_upper))
            .map(|s| s.to_string())
            .collect();
        (start, candidates)
    }
}

/// A highlighter that upper-cases the line -- visibly different from the
/// built-in keyword one, but (like any real `Highlighter`) preserves the
/// exact char count so cursor positioning stays correct.
struct TestHighlighter;

impl Highlighter for TestHighlighter {
    fn highlight(&self, line: &str) -> String {
        line.to_uppercase()
    }
}

fn main() {
    let mut ed = Readline::new();
    if std::env::var("DB_CLI_TEST_COMPLETER").is_ok() {
        ed.set_completer(TestCompleter);
    }
    if std::env::var("DB_CLI_TEST_HIGHLIGHTER").is_ok() {
        ed.set_highlighter(TestHighlighter);
    }
    loop {
        match ed.read_line("column> ") {
            Ok(line) if line.trim() == "quit" => break,
            Ok(line) => ed.add_history_entry(&line),
            Err(_) => break,
        }
    }
}
