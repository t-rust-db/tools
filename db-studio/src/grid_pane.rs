//! The big-right pane (db-studio#4): renders a query result set as a
//! scrollable grid, with a scrollbar alongside the existing row highlight
//! (db-studio#12). `Grid`'s cells are already-formatted strings rather
//! than `db-core`'s `Value` -- mode-agnostic rendering per
//! `.openspec/plan.md`'s Layout section, so this pane doesn't need to
//! know which engine (row/batch/stream) produced the data. Converting a
//! real `Value` into `Grid` cells is #2/#6's job, once
//! t-rust-db/db-core#295 lands.

use ratatui::layout::{Constraint, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState};
use ratatui::Frame;

use crate::theme;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grid {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Grid {
    pub fn new(headers: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Self { headers, rows }
    }
}

#[derive(Default)]
pub struct GridPane {
    grid: Grid,
    state: TableState,
    /// Index of the leftmost visible column (db-studio#42) -- a wide
    /// result set (e.g. `SELECT *` over a log's Tier-3 columns) squeezed
    /// every column to unreadable widths with no way to see a truncated
    /// one. Scrolling right hides that many leading columns rather than
    /// shrinking them further, so the remaining ones stay readable.
    col_offset: usize,
}

impl GridPane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the displayed result set, resetting both scroll axes.
    pub fn set_grid(&mut self, grid: Grid) {
        self.grid = grid;
        self.state = TableState::default();
        self.col_offset = 0;
        if !self.grid.rows.is_empty() {
            self.state.select(Some(0));
        }
    }

    pub fn clear(&mut self) {
        self.set_grid(Grid::default());
    }

    pub fn scroll_down(&mut self) {
        if self.grid.rows.is_empty() {
            return;
        }
        let next = self.state.selected().map_or(0, |i| {
            usize::min(i.saturating_add(1), self.grid.rows.len() - 1)
        });
        self.state.select(Some(next));
    }

    pub fn scroll_up(&mut self) {
        if self.grid.rows.is_empty() {
            return;
        }
        let prev = self.state.selected().map_or(0, |i| i.saturating_sub(1));
        self.state.select(Some(prev));
    }

    /// Hides one more leading column, stopping once a single column
    /// (the last one) would remain -- there's always something to show.
    pub fn scroll_right(&mut self) {
        let last = self.grid.headers.len().saturating_sub(1);
        self.col_offset = usize::min(self.col_offset.saturating_add(1), last);
    }

    pub fn scroll_left(&mut self) {
        self.col_offset = self.col_offset.saturating_sub(1);
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let offset = self.col_offset.min(self.grid.headers.len());
        let visible_headers = self.grid.headers.get(offset..).unwrap_or_default();
        let header = Row::new(visible_headers.to_vec()).style(
            Style::default()
                .fg(theme::header())
                .add_modifier(Modifier::BOLD),
        );
        let rows = self.grid.rows.iter().map(|r| {
            let cells = r.get(offset..).unwrap_or_default();
            Row::new(cells.to_vec())
        });
        let widths: Vec<Constraint> = if visible_headers.is_empty() {
            vec![Constraint::Percentage(100)]
        } else {
            vec![Constraint::Ratio(1, visible_headers.len() as u32); visible_headers.len()]
        };
        // Scrolled-right state is otherwise invisible (no horizontal
        // scrollbar) -- the title is the only cue that columns to the
        // left are hidden, same spirit as the vertical scrollbar cueing
        // there's more above/below.
        let title = if offset > 0 {
            format!("results -- {offset} column(s) hidden to the left")
        } else {
            "results".to_string()
        };
        let table = Table::new(rows, widths)
            .header(header)
            .block(theme::pane_block(title, false))
            .row_highlight_style(
                Style::default()
                    .bg(theme::selection_bg())
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(table, area, &mut self.state);

        if self.grid.rows.len() > 1 {
            let mut scrollbar_state = ScrollbarState::new(self.grid.rows.len())
                .position(self.state.selected().unwrap_or(0));
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Grid {
        Grid::new(
            vec!["a".into(), "b".into()],
            vec![
                vec!["1".into(), "2".into()],
                vec!["3".into(), "4".into()],
                vec!["5".into(), "6".into()],
            ],
        )
    }

    #[test]
    fn set_grid_selects_the_first_row_when_non_empty() {
        let mut pane = GridPane::new();
        pane.set_grid(sample());
        assert_eq!(pane.state.selected(), Some(0));
    }

    #[test]
    fn set_grid_selects_nothing_when_empty() {
        let mut pane = GridPane::new();
        pane.set_grid(Grid::default());
        assert_eq!(pane.state.selected(), None);
    }

    #[test]
    fn scroll_down_stops_at_the_last_row() {
        let mut pane = GridPane::new();
        pane.set_grid(sample());
        pane.scroll_down();
        pane.scroll_down();
        pane.scroll_down();
        pane.scroll_down();
        assert_eq!(pane.state.selected(), Some(2));
    }

    #[test]
    fn scroll_up_stops_at_the_first_row() {
        let mut pane = GridPane::new();
        pane.set_grid(sample());
        pane.scroll_up();
        pane.scroll_up();
        assert_eq!(pane.state.selected(), Some(0));
    }

    #[test]
    fn clear_resets_to_an_empty_grid_with_no_selection() {
        let mut pane = GridPane::new();
        pane.set_grid(sample());
        pane.clear();
        assert_eq!(pane.grid, Grid::default());
        assert_eq!(pane.state.selected(), None);
    }

    fn wide_sample() -> Grid {
        Grid::new(
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            vec![vec!["1".into(), "2".into(), "3".into(), "4".into()]],
        )
    }

    #[test]
    fn scroll_right_stops_leaving_the_last_column_visible() {
        let mut pane = GridPane::new();
        pane.set_grid(wide_sample());
        for _ in 0..10 {
            pane.scroll_right();
        }
        assert_eq!(pane.col_offset, 3);
    }

    #[test]
    fn scroll_left_stops_at_zero() {
        let mut pane = GridPane::new();
        pane.set_grid(wide_sample());
        pane.scroll_right();
        pane.scroll_right();
        pane.scroll_left();
        pane.scroll_left();
        pane.scroll_left();
        assert_eq!(pane.col_offset, 0);
    }

    #[test]
    fn set_grid_resets_the_column_offset() {
        let mut pane = GridPane::new();
        pane.set_grid(wide_sample());
        pane.scroll_right();
        pane.scroll_right();
        pane.set_grid(wide_sample());
        assert_eq!(pane.col_offset, 0);
    }

    #[allow(
        clippy::unwrap_used,
        reason = "test code fails fast -- see db-core's own test files for the same convention"
    )]
    #[test]
    fn scrolling_right_hides_leading_columns_and_shows_a_hint() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut pane = GridPane::new();
        pane.set_grid(wide_sample());
        pane.scroll_right();
        pane.scroll_right();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, frame.area()))
            .unwrap();
        let content = terminal.backend().buffer().content();
        let rendered: String = content.iter().map(|cell| cell.symbol()).collect();

        assert!(
            !rendered.contains('a') && !rendered.contains('1'),
            "expected hidden leading columns not to render:\n{rendered}"
        );
        assert!(
            rendered.contains('c') && rendered.contains('3'),
            "expected a visible column to still render:\n{rendered}"
        );
        assert!(
            rendered.contains("2 column(s) hidden"),
            "expected a hint about hidden columns in the title:\n{rendered}"
        );
    }
}
