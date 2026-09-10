//! The bottom-of-screen status bar (db-studio#19): active file, mode,
//! and the query pane's cursor position. No state of its own -- a plain
//! render function, not a pane type, since there's nothing here to hold
//! between frames.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme;

pub fn render(frame: &mut Frame, area: Rect, file_label: &str, mode: &str, cursor: (usize, usize)) {
    let (row, col) = cursor;
    let text = format!(
        " {file_label}  |  mode: {mode}  |  ln {}, col {} ",
        row + 1,
        col + 1
    );
    let paragraph =
        Paragraph::new(text).style(Style::default().bg(theme::base()).fg(theme::subtext()));
    frame.render_widget(paragraph, area);
}
