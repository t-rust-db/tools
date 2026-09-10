//! The app loop, wired end-to-end (db-studio#6), with visual polish and
//! a real editor for the query pane (db-studio#9/#12): submit a query ->
//! run it against the open file's `Engine` -> grid or error pane -> quit
//! cleanly. `Box<dyn Engine>` rather than a concrete `RowEngine`, even
//! though M1 only ever opens one -- that's the seam M3 needs to switch
//! engines per open file (t-rust-db/db-core#295), and there is no cost
//! to holding it from the start.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use db_core::engine::Engine;
use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::error_pane::ErrorPane;
use crate::grid_pane::{Grid, GridPane};
use crate::query_pane::QueryPane;
use crate::terminal::Tui;

pub struct App {
    running: bool,
    engine: Box<dyn Engine>,
    query_pane: QueryPane,
    grid_pane: GridPane,
    error_pane: ErrorPane,
}

impl App {
    pub fn new(engine: Box<dyn Engine>) -> Self {
        Self {
            running: true,
            engine,
            query_pane: QueryPane::new(),
            grid_pane: GridPane::new(),
            error_pane: ErrorPane::new(),
        }
    }

    pub fn run(&mut self, terminal: &mut Tui) -> io::Result<()> {
        while self.running {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [query_area, grid_area, error_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .areas(frame.area());
        // The query pane is the only focusable pane until #10's schema
        // tree inspector adds a second one -- hardcoded `true` rather
        // than unused focus-cycling machinery for a single pane.
        self.query_pane.render(frame, query_area, true);
        self.grid_pane.render(frame, grid_area);
        self.error_pane.render(frame, error_area);
    }

    fn handle_events(&mut self) -> io::Result<()> {
        // Bounded poll so the loop can react to a resize/redraw even with
        // no key pressed, without spinning the CPU.
        if !event::poll(Duration::from_millis(250))? {
            return Ok(());
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            // `q` is a valid SQL character, so it can no longer double as
            // quit now that the query pane accepts arbitrary text (#1's
            // scaffold had no text input yet, so it was safe there).
            let is_ctrl_c =
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
            if is_ctrl_c || key.code == KeyCode::Esc {
                self.running = false;
                return Ok(());
            }
            // PageUp/PageDown scroll the grid -- plain Up/Down now move
            // the query pane's cursor within its (possibly multi-line,
            // #9) buffer instead, so they can no longer double as the
            // grid's scroll keys the way M1's single-line pane allowed.
            // Handled here and not forwarded to the query pane, which
            // would otherwise also treat them as its own scroll keys.
            match key.code {
                KeyCode::PageDown => {
                    self.grid_pane.scroll_down();
                    return Ok(());
                }
                KeyCode::PageUp => {
                    self.grid_pane.scroll_up();
                    return Ok(());
                }
                _ => {}
            }
            if let Some(query) = self.query_pane.handle_key(key) {
                self.submit(&query);
            }
        }
        Ok(())
    }

    fn submit(&mut self, query: &str) {
        match self.engine.run_query(query) {
            Ok(result) => {
                let headers = result.columns;
                let rows = result
                    .rows
                    .into_iter()
                    .map(|row| row.iter().map(ToString::to_string).collect())
                    .collect();
                self.grid_pane.set_grid(Grid::new(headers, rows));
                self.error_pane.clear();
            }
            Err(err) => {
                self.grid_pane.clear();
                self.error_pane.set_error(err.to_string());
            }
        }
    }
}
