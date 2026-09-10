//! The top pane (db-studio#3): a single-line SQL input, submitted on Enter.
//! No syntax highlighting yet -- see `.openspec/plan.md`'s Layout section
//! and M6.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

#[derive(Default)]
pub struct QueryPane {
    text: String,
    cursor: usize,
}

impl QueryPane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles one key event. Returns the submitted query text on Enter
    /// (and clears the pane), otherwise `None`.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        match key.code {
            KeyCode::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                None
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let prev = self.text[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.text.drain(prev..self.cursor);
                self.cursor = prev;
                None
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor = self.text[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                None
            }
            KeyCode::Right if self.cursor < self.text.len() => {
                self.cursor += self.text[self.cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0);
                None
            }
            KeyCode::Enter if !self.text.is_empty() => {
                let submitted = std::mem::take(&mut self.text);
                self.cursor = 0;
                Some(submitted)
            }
            _ => None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .title("query")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White));
        frame.render_widget(Paragraph::new(self.text.as_str()).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        let mut event = KeyEvent::new(code, KeyModifiers::NONE);
        event.kind = KeyEventKind::Press;
        event
    }

    #[test]
    fn typing_appends_and_enter_submits_then_clears() {
        let mut pane = QueryPane::new();
        for c in "SELECT 1".chars() {
            assert_eq!(pane.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(pane.text, "SELECT 1");
        assert_eq!(
            pane.handle_key(key(KeyCode::Enter)),
            Some("SELECT 1".to_string())
        );
        assert_eq!(pane.text, "");
    }

    #[test]
    fn enter_on_empty_text_does_not_submit() {
        let mut pane = QueryPane::new();
        assert_eq!(pane.handle_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn backspace_removes_the_preceding_char() {
        let mut pane = QueryPane::new();
        pane.handle_key(key(KeyCode::Char('a')));
        pane.handle_key(key(KeyCode::Char('b')));
        pane.handle_key(key(KeyCode::Backspace));
        assert_eq!(pane.text, "a");
    }

    #[test]
    fn cursor_left_then_insert_places_text_mid_string() {
        let mut pane = QueryPane::new();
        pane.handle_key(key(KeyCode::Char('a')));
        pane.handle_key(key(KeyCode::Char('c')));
        pane.handle_key(key(KeyCode::Left));
        pane.handle_key(key(KeyCode::Char('b')));
        assert_eq!(pane.text, "abc");
    }
}
