//! The top pane (db-studio#9): a `tui-textarea-2`-backed multi-line SQL
//! editor with syntax highlighting from `db-core`'s own tokenizer (see
//! `highlight.rs`). Enter inserts a newline; `F5` submits --
//! `QueryPane::new`'s single-line hand-rolled cursor logic is gone.
//!
//! `F5`, not `Ctrl+Enter`: a plain terminal (no Kitty keyboard protocol --
//! macOS Terminal.app and a generic xterm included) cannot distinguish
//! `Ctrl+Enter` from plain `Enter` at the byte level, both send the same
//! carriage return, so that binding would be unreachable on most real
//! terminals. A function key has its own distinct escape sequence
//! everywhere and isn't shadowed by any of `tui-textarea-2`'s own
//! bindings (unlike e.g. `Ctrl+R`, which is its `redo`).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::Frame;
use tui_textarea::TextArea;

use crate::highlight;
use crate::theme;

pub struct QueryPane {
    textarea: TextArea<'static>,
}

impl QueryPane {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_selection_style(ratatui::style::Style::default().bg(theme::selection_bg()));
        Self { textarea }
    }

    /// Handles one key event. Returns the submitted query text on `F5`
    /// (and clears the buffer), otherwise `None`.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.code == KeyCode::F(5) {
            let text = self.textarea.lines().join("\n");
            if text.trim().is_empty() {
                return None;
            }
            self.textarea = TextArea::default();
            self.textarea
                .set_selection_style(ratatui::style::Style::default().bg(theme::selection_bg()));
            return Some(text);
        }
        self.textarea.input(key);
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        self.textarea.clear_custom_highlight();
        let text = self.textarea.lines().join("\n");
        for (start, end, style) in highlight::highlights(&text) {
            self.textarea.custom_highlight((start, end), style, 10);
        }
        self.textarea.set_block(theme::pane_block("query", focused));
        frame.render_widget(&self.textarea, area);
    }
}

impl Default for QueryPane {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn f5() -> KeyEvent {
        KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)
    }

    #[test]
    fn typing_then_f5_submits_and_clears() {
        let mut pane = QueryPane::new();
        for c in "SELECT 1".chars() {
            assert_eq!(pane.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(pane.handle_key(f5()), Some("SELECT 1".to_string()));
        assert_eq!(pane.textarea.lines(), &[""]);
    }

    #[test]
    fn plain_enter_inserts_a_newline_rather_than_submitting() {
        let mut pane = QueryPane::new();
        pane.handle_key(key(KeyCode::Char('a')));
        assert_eq!(pane.handle_key(key(KeyCode::Enter)), None);
        pane.handle_key(key(KeyCode::Char('b')));
        assert_eq!(pane.textarea.lines(), &["a", "b"]);
    }

    #[test]
    fn f5_on_blank_input_does_not_submit() {
        let mut pane = QueryPane::new();
        assert_eq!(pane.handle_key(f5()), None);
    }

    #[test]
    fn f5_on_whitespace_only_input_does_not_submit() {
        let mut pane = QueryPane::new();
        pane.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(pane.handle_key(f5()), None);
    }
}
