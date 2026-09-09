//! The big-right pane (db-studio#4): renders a query result set as a
//! scrollable grid. `Grid`'s cells are already-formatted strings rather
//! than `db-core`'s `Value` -- mode-agnostic rendering per
//! `.openspec/plan.md`'s Layout section, so this pane doesn't need to
//! know which engine (row/batch/stream) produced the data. Converting a
//! real `Value` into `Grid` cells is #2/#6's job, once
//! t-rust-db/db-core#295 lands.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Row, Table, TableState};
use ratatui::Frame;

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
}

impl GridPane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the displayed result set, resetting scroll to the top.
    pub fn set_grid(&mut self, grid: Grid) {
        self.grid = grid;
        self.state = TableState::default();
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(self.grid.headers.clone())
            .style(Style::default().add_modifier(Modifier::BOLD));
        let rows = self.grid.rows.iter().map(|r| Row::new(r.clone()));
        let widths: Vec<Constraint> = if self.grid.headers.is_empty() {
            vec![Constraint::Percentage(100)]
        } else {
            vec![Constraint::Ratio(1, self.grid.headers.len() as u32); self.grid.headers.len()]
        };
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().title("results").borders(Borders::ALL))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(table, area, &mut self.state);
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
}
