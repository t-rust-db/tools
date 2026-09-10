//! The app loop, wired end-to-end (db-studio#6), with visual polish, a
//! real editor with completion for the query pane (db-studio#9/#12/#11),
//! a schema tree (db-studio#10), and multiple open files (db-studio#16):
//! submit a query -> run it against the active file's `Engine` -> grid
//! or error pane -> quit cleanly. Each file's `Box<dyn Engine>` rather
//! than a concrete `RowEngine`, even though M2 only ever opens row-mode
//! files -- that's the seam M3 needs to switch engines per open file
//! (t-rust-db/db-core#295), and there is no cost to holding it from the
//! start.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use db_core::engine::Engine;
use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::completion;
use crate::error_pane::ErrorPane;
use crate::grid_pane::{Grid, GridPane};
use crate::query_pane::QueryPane;
use crate::schema_tree::SchemaTreePane;
use crate::terminal::Tui;

/// One file opened on the command line, per db-studio#16 -- `main.rs`
/// builds these before the terminal is touched (a bad path is a plain
/// stderr message, same as M1's single-file convention).
pub struct OpenFile {
    #[allow(
        dead_code,
        reason = "read by #17's file-rooted tree labels, not yet wired"
    )]
    pub path: PathBuf,
    pub engine: Box<dyn Engine>,
}

/// Which pane has keyboard focus. Grid/error aren't in this enum -- they
/// take no directional/edit input of their own, only the global
/// PageUp/PageDown scroll keys, which work regardless of focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Query,
    Tree,
}

pub struct App {
    running: bool,
    files: Vec<OpenFile>,
    /// Index into `files` of the file queries currently run against.
    /// Always valid: `App::new` requires a non-empty `files`, and
    /// nothing removes entries from it (files are only ever added, not
    /// closed, in M2's scope).
    active: usize,
    focus: Focus,
    query_pane: QueryPane,
    schema_tree: SchemaTreePane,
    grid_pane: GridPane,
    error_pane: ErrorPane,
}

impl App {
    /// `files` must be non-empty -- `main.rs`'s own usage-message path
    /// handles the zero-files case before ever constructing an `App`.
    pub fn new(files: Vec<OpenFile>) -> Self {
        // A file that fails `tables()` still opens -- an empty tree (and
        // an empty completion candidate list) rather than refusing to
        // start, same spirit as an empty query result rendering as an
        // empty grid rather than an error.
        let tables = files
            .first()
            .map(|f| f.engine.tables().unwrap_or_default())
            .unwrap_or_default();
        let candidates = completion::candidates(&tables);
        Self {
            running: true,
            files,
            active: 0,
            focus: Focus::Query,
            query_pane: QueryPane::new(candidates),
            schema_tree: SchemaTreePane::new(tables),
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
        let [query_area, middle_area, error_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .areas(frame.area());
        let [tree_area, grid_area] =
            Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(middle_area);
        self.query_pane
            .render(frame, query_area, self.focus == Focus::Query);
        self.schema_tree
            .render(frame, tree_area, self.focus == Focus::Tree);
        self.grid_pane.render(frame, grid_area);
        self.error_pane.render(frame, error_area);
        // Last: ratatui has no z-ordering, so the completion popup must
        // paint after every pane it might overlap, not before.
        self.query_pane.render_popup(frame, query_area);
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
            // A query-pane completion popup claims Esc (close it) and Tab
            // (accept the selection) for itself before either reaches
            // this function's own quit/focus-cycle handling below.
            let query_has_popup = self.focus == Focus::Query && self.query_pane.has_open_popup();

            // `q` is a valid SQL character, so it can no longer double as
            // quit now that the query pane accepts arbitrary text (#1's
            // scaffold had no text input yet, so it was safe there).
            let is_ctrl_c =
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
            if is_ctrl_c || (key.code == KeyCode::Esc && !query_has_popup) {
                self.running = false;
                return Ok(());
            }
            if key.code == KeyCode::Tab && !query_has_popup {
                self.focus = match self.focus {
                    Focus::Query => Focus::Tree,
                    Focus::Tree => Focus::Query,
                };
                return Ok(());
            }
            // PageUp/PageDown scroll the grid regardless of focus -- the
            // grid itself isn't focusable, and nothing else claims these
            // keys.
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
            match self.focus {
                Focus::Query => {
                    if let Some(query) = self.query_pane.handle_key(key) {
                        self.submit(&query);
                    }
                }
                Focus::Tree => self.schema_tree.handle_key(key),
            }
        }
        Ok(())
    }

    fn submit(&mut self, query: &str) {
        let Some(file) = self.files.get_mut(self.active) else {
            return;
        };
        match file.engine.run_query(query) {
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
