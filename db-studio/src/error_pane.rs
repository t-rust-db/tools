//! The bottom pane (db-studio#5): renders a parse/execution error from the
//! same query-submission path the grid pane uses -- no separate
//! error-handling code path, per the issue's acceptance criteria.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::theme;

#[derive(Default)]
pub struct ErrorPane {
    message: Option<String>,
}

impl ErrorPane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_error(&mut self, message: String) {
        self.message = Some(message);
    }

    /// Submitting a new (successful) query clears any previously shown
    /// error, per the issue's acceptance criteria.
    pub fn clear(&mut self) {
        self.message = None;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let text = self.message.as_deref().unwrap_or("");
        let paragraph = Paragraph::new(text)
            .style(Style::default().fg(theme::error()))
            .wrap(Wrap { trim: false })
            .block(theme::pane_block_borderless());
        frame.render_widget(paragraph, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_error_then_clear_round_trips() {
        let mut pane = ErrorPane::new();
        pane.set_error("boom".to_string());
        assert_eq!(pane.message.as_deref(), Some("boom"));
        pane.clear();
        assert_eq!(pane.message, None);
    }
}
