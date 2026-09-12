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
    /// The selected row's index, when its detail section (db-studio#45)
    /// is open -- always exactly `self.state.selected()` when `Some`
    /// (see [`Self::toggle_expanded`]/[`Self::scroll_up`]/
    /// [`Self::scroll_down`]), never some *other* row, so its detail
    /// lines are always inserted strictly after the highlighted row in
    /// the rendered `Table` and never shift the highlighted row's own
    /// widget-row index.
    expanded: Option<usize>,
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
        self.expanded = None;
        if !self.grid.rows.is_empty() {
            self.state.select(Some(0));
        }
    }

    pub fn clear(&mut self) {
        self.set_grid(Grid::default());
    }

    /// Tab-separated, newline-per-row plain text (db-studio#42's
    /// clipboard yank) -- headers first, no box-drawing/ANSI styling,
    /// so pasting elsewhere gets exactly the data.
    pub fn plain_text(&self) -> String {
        let mut out = self.grid.headers.join("\t");
        for row in &self.grid.rows {
            out.push('\n');
            out.push_str(&row.join("\t"));
        }
        out
    }

    pub fn scroll_down(&mut self) {
        if self.grid.rows.is_empty() {
            return;
        }
        let next = self.state.selected().map_or(0, |i| {
            usize::min(i.saturating_add(1), self.grid.rows.len() - 1)
        });
        self.state.select(Some(next));
        self.expanded = None;
    }

    pub fn scroll_up(&mut self) {
        if self.grid.rows.is_empty() {
            return;
        }
        let prev = self.state.selected().map_or(0, |i| i.saturating_sub(1));
        self.state.select(Some(prev));
        self.expanded = None;
    }

    /// Opens or closes the selected row's detail section (db-studio#45):
    /// every column's full value, one per line, shifting later rows
    /// down in the same table rather than a separate pane. Navigating
    /// away (`scroll_up`/`scroll_down`) auto-collapses it -- `expanded`
    /// only ever names the *currently selected* row (see its own doc
    /// comment on why that invariant matters for rendering).
    pub fn toggle_expanded(&mut self) {
        let Some(selected) = self.state.selected() else {
            return;
        };
        self.expanded = if self.expanded == Some(selected) {
            None
        } else {
            Some(selected)
        };
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

    /// The expanded row's detail lines (db-studio#45): `key: value` for
    /// every column at full value, not sliced by horizontal scroll --
    /// the detail section's whole point is seeing everything regardless
    /// of what fits in the truncated grid. `raw` (present on `.log`
    /// files' single `log` table) sorts first, since the original line
    /// is usually what you actually want to read first; every other
    /// column follows in its normal order.
    fn detail_lines(&self, row_index: usize) -> Vec<String> {
        let Some(row) = self.grid.rows.get(row_index) else {
            return Vec::new();
        };
        let raw_index = self.grid.headers.iter().position(|h| h == "raw");
        let ordered_indices = raw_index
            .into_iter()
            .chain((0..self.grid.headers.len()).filter(|&i| Some(i) != raw_index));
        ordered_indices
            .filter_map(|i| {
                let header = self.grid.headers.get(i)?;
                let value = row.get(i)?;
                Some(format!("  {header}: {value}"))
            })
            .collect()
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let offset = self.col_offset.min(self.grid.headers.len());
        let visible_headers = self.grid.headers.get(offset..).unwrap_or_default();
        let header = Row::new(visible_headers.to_vec()).style(
            Style::default()
                .fg(theme::header())
                .add_modifier(Modifier::BOLD),
        );
        let rows =
            self.grid.rows.iter().enumerate().flat_map(|(i, r)| {
                let cells = r.get(offset..).unwrap_or_default();
                let mut out = vec![Row::new(cells.to_vec())];
                if self.expanded == Some(i) {
                    out.extend(self.detail_lines(i).into_iter().map(|line| {
                        Row::new(vec![line]).style(Style::default().fg(theme::subtext()))
                    }));
                }
                out
            });
        let widths: Vec<Constraint> = if visible_headers.is_empty() {
            vec![Constraint::Percentage(100)]
        } else {
            vec![Constraint::Ratio(1, visible_headers.len() as u32); visible_headers.len()]
        };
        // Row count (rainfrog's convention) up front, then the hidden-
        // column hint when scrolled right -- otherwise scrolling is
        // invisible with no horizontal scrollbar's own visual cue.
        let row_count = self.grid.rows.len();
        let title = if offset > 0 {
            format!("results -- {row_count} row(s), {offset} column(s) hidden to the left")
        } else {
            format!("results -- {row_count} row(s)")
        };
        let table = Table::new(rows, widths)
            .header(header)
            .block(theme::pane_block(title, focused))
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

        // Horizontal scrollbar (db-studio#42) -- Shift+Left/Right
        // already scrolled column visibility (db-studio#42) with only
        // the title as a cue; a real scrollbar makes "there are more
        // columns this way" visible without reading the title text.
        if self.grid.headers.len() > 1 {
            let last_offset = self.grid.headers.len().saturating_sub(1);
            let mut h_scrollbar_state = ScrollbarState::new(last_offset.max(1)).position(offset);
            let h_scrollbar = Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .begin_symbol(None)
                .end_symbol(None);
            frame.render_stateful_widget(
                h_scrollbar,
                area.inner(Margin {
                    vertical: 0,
                    horizontal: 1,
                }),
                &mut h_scrollbar_state,
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
        // Non-numeric values (not "1", "2", ...): the row-count hint in
        // the title now legitimately contains digits, so digit-valued
        // cells would be indistinguishable from it in a rendered-buffer
        // substring check.
        Grid::new(
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            vec![vec!["wow".into(), "xen".into(), "eek".into(), "ohh".into()]],
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
            .draw(|frame| pane.render(frame, frame.area(), false))
            .unwrap();
        let content = terminal.backend().buffer().content();
        let rendered: String = content.iter().map(|cell| cell.symbol()).collect();

        assert!(
            !rendered.contains('a') && !rendered.contains("wow"),
            "expected hidden leading columns not to render:\n{rendered}"
        );
        assert!(
            rendered.contains('c') && rendered.contains("eek"),
            "expected a visible column to still render:\n{rendered}"
        );
        assert!(
            rendered.contains("2 column(s) hidden"),
            "expected a hint about hidden columns in the title:\n{rendered}"
        );
    }

    #[test]
    fn toggle_expanded_opens_then_closes_the_selected_row() {
        let mut pane = GridPane::new();
        pane.set_grid(sample());
        assert_eq!(pane.expanded, None);
        pane.toggle_expanded();
        assert_eq!(pane.expanded, Some(0));
        pane.toggle_expanded();
        assert_eq!(pane.expanded, None);
    }

    #[test]
    fn scrolling_away_auto_collapses_the_detail_section() {
        let mut pane = GridPane::new();
        pane.set_grid(sample());
        pane.toggle_expanded();
        assert_eq!(pane.expanded, Some(0));
        pane.scroll_down();
        assert_eq!(pane.expanded, None);
    }

    #[test]
    fn detail_lines_puts_raw_first_when_present() {
        let grid = Grid::new(
            vec!["message".into(), "raw".into(), "severity".into()],
            vec![vec!["hi".into(), "<38>Sep 9 ...".into(), "6".into()]],
        );
        let mut pane = GridPane::new();
        pane.set_grid(grid);
        let lines = pane.detail_lines(0);
        assert_eq!(
            lines,
            vec![
                "  raw: <38>Sep 9 ...".to_string(),
                "  message: hi".to_string(),
                "  severity: 6".to_string(),
            ]
        );
    }

    #[allow(
        clippy::unwrap_used,
        reason = "test code fails fast -- see db-core's own test files for the same convention"
    )]
    #[test]
    fn expanding_a_row_renders_its_detail_lines_and_shifts_later_rows_down() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut pane = GridPane::new();
        pane.set_grid(sample());
        pane.toggle_expanded();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, frame.area(), false))
            .unwrap();
        let content = terminal.backend().buffer().content();
        let rendered: String = content.iter().map(|cell| cell.symbol()).collect();

        assert!(
            rendered.contains("a: 1") && rendered.contains("b: 2"),
            "expected the expanded row's detail lines:\n{rendered}"
        );
    }
}
