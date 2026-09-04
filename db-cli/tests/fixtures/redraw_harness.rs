//! Minimal `read_line` loop driven over a pty by `tests/redraw_pty.rs`
//! (issue #1) to exercise the wrapped-line redraw path against a real
//! (virtual) terminal, not just `layout`'s pure math in isolation.

use db_cli::editor::Readline;

fn main() {
    let mut ed = Readline::new();
    loop {
        match ed.read_line("column> ") {
            Ok(line) if line.trim() == "quit" => break,
            Ok(line) => ed.add_history_entry(&line),
            Err(_) => break,
        }
    }
}
