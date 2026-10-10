//! The bottom-of-screen status bar (db-studio#19): active file, mode,
//! and the query pane's cursor position on the left; the running
//! version on the right. No state of its own -- a plain render
//! function, not a pane type, since there's nothing here to hold
//! between frames.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    file_label: &str,
    mode: &str,
    cursor: (usize, usize),
    version: &str,
) {
    let (row, col) = cursor;
    let left_text = format!(
        " {file_label}  |  mode: {mode}  |  ln {}, col {} ",
        row + 1,
        col + 1
    );
    let right_text = format!(" v{version} ");

    let style = Style::default().bg(theme::base()).fg(theme::subtext());
    // The right chunk is exactly as wide as its own text (plus the
    // leading/trailing space baked into right_text) so the version
    // sits flush against the right edge, not centered in leftover
    // space -- the rest of the bar's width goes to the left side.
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::try_from(right_text.chars().count()).unwrap_or(u16::MAX)),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(left_text).style(style), left_area);
    frame.render_widget(
        Paragraph::new(right_text)
            .style(style)
            .alignment(Alignment::Right),
        right_area,
    );
}
