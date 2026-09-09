//! The app loop wiring query -> grid/error together (db-studio#4/#5).
//! `fake_run_query` is scaffolding standing in for #2's real engine
//! hookup (blocked on t-rust-db/db-core#295) -- #6 replaces it with a
//! real `Engine` call, at which point this function is deleted, not kept
//! around as a fallback.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::error_pane::ErrorPane;
use crate::grid_pane::{Grid, GridPane};
use crate::query_pane::QueryPane;
use crate::terminal::Tui;

pub struct App {
    running: bool,
    query_pane: QueryPane,
    grid_pane: GridPane,
    error_pane: ErrorPane,
}

impl App {
    pub fn new() -> Self {
        Self {
            running: true,
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
        self.query_pane.render(frame, query_area);
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
            match key.code {
                KeyCode::Down => self.grid_pane.scroll_down(),
                KeyCode::Up => self.grid_pane.scroll_up(),
                _ => {}
            }
            if let Some(query) = self.query_pane.handle_key(key) {
                self.submit(&query);
            }
        }
        Ok(())
    }

    fn submit(&mut self, query: &str) {
        match fake_run_query(query) {
            Ok(grid) => {
                self.grid_pane.set_grid(grid);
                self.error_pane.clear();
            }
            Err(message) => {
                self.grid_pane.clear();
                self.error_pane.set_error(message);
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Scaffolding, not real query execution -- see this module's doc comment.
fn fake_run_query(query: &str) -> Result<Grid, String> {
    if query.to_ascii_lowercase().contains("error") {
        return Err(format!("no such table (fake engine, #2 pending): {query}"));
    }
    Ok(Grid::new(
        vec!["query".to_string(), "note".to_string()],
        vec![vec![
            query.to_string(),
            "fake result -- db-core#295/#2 pending".to_string(),
        ]],
    ))
}
