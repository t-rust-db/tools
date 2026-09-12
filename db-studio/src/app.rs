//! The app loop, wired end-to-end (db-studio#6), with visual polish, a
//! real editor with completion for the query pane (db-studio#9/#12/#11),
//! a schema tree (db-studio#10), multiple open files (db-studio#16), two
//! engine modes (db-studio#23), and introspection views (db-studio#29):
//! submit a query -> run it against the active file's `Engine` -> grid
//! or error pane -> quit cleanly. Each file's `Box<dyn Engine>` rather
//! than a concrete `RowEngine`/`BatchEngine` -- the seam that lets M3
//! switch engines per open file (t-rust-db/db-core#295), with no cost to
//! holding it from the start.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use db_core::engine::{Cell, Engine};
use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::completion;
use crate::error_pane::ErrorPane;
use crate::grid_pane::{Grid, GridPane};
use crate::open::open_by_extension;
use crate::query_pane::QueryPane;
use crate::schema_tree::{FileSchema, SchemaTreePane};
use crate::terminal::Tui;
use crate::{clipboard, opcode_pane, plan_pane, stats_pane, status_bar, theme};
use db_core::engine::{OpcodeSection, PlanRow};

/// One file opened on the command line, per db-studio#16 -- `main.rs`
/// builds these before the terminal is touched (a bad path is a plain
/// stderr message, same as M1's single-file convention).
pub struct OpenFile {
    pub path: PathBuf,
    pub engine: Box<dyn Engine>,
}

/// The tree's display label for a file -- its filename, not the full
/// path (the path is still the tree's unique root *key*, just not what
/// the user reads).
fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A grid cell's display text (db-studio#44) -- `Cell::Display`
/// itself renders `NULL` as empty, correct for db-core's shell-style
/// clients (sqlite-rs/column-rs/loglume, matching `sqlite3`'s own CLI
/// convention), but that makes a `NULL` indistinguishable from a real
/// empty string in a grid meant to be looked at rather than piped.
/// Only `Cell::Null` gets the special case; every other variant still
/// renders exactly as `Cell::Display` already does.
fn cell_display(cell: &Cell) -> String {
    match cell {
        Cell::Null => "<NULL>".to_string(),
        other => other.to_string(),
    }
}

/// Which pane has keyboard focus. Grid/error aren't in this enum -- they
/// take no directional/edit input of their own, only the global
/// PageUp/PageDown scroll keys, which work regardless of focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Query,
    Tree,
}

/// Which view the big-right area shows (db-studio#29). `Plan`/`Opcodes`
/// hold their last-computed data -- refreshed each time their key is
/// pressed (even if already showing that view, so editing the query and
/// pressing the key again updates it), not on every frame, since
/// computing them re-parses/re-compiles the query pane's current text.
enum OutputView {
    Results,
    Plan(Vec<PlanRow>),
    Opcodes(Vec<OpcodeSection>),
    Stats,
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
    view: OutputView,
    query_pane: QueryPane,
    schema_tree: SchemaTreePane,
    grid_pane: GridPane,
    error_pane: ErrorPane,
    /// The `Ctrl+O` open-file prompt's text, while it's open
    /// (db-studio#42) -- `None` means it isn't, and every key reaches
    /// its normal destination instead of this buffer.
    open_prompt: Option<String>,
}

impl App {
    /// `files` must be non-empty -- `main.rs`'s own usage-message path
    /// handles the zero-files case before ever constructing an `App`.
    pub fn new(files: Vec<OpenFile>) -> Self {
        // A file that fails `tables()` still opens -- an empty branch of
        // the tree for it rather than refusing to start, same spirit as
        // an empty query result rendering as an empty grid rather than
        // an error.
        let file_schemas: Vec<FileSchema> = files
            .iter()
            .map(|f| FileSchema {
                key: f.path.display().to_string(),
                label: file_label(&f.path),
                tables: f.engine.tables().unwrap_or_default(),
            })
            .collect();
        // Completion offers the first (initially active) file's names;
        // switch_active_file (db-studio#18) recomputes this when the
        // active file changes.
        let candidates = files
            .first()
            .map(|f| completion::candidates(&f.engine.tables().unwrap_or_default()))
            .unwrap_or_default();
        Self {
            running: true,
            files,
            active: 0,
            focus: Focus::Query,
            view: OutputView::Results,
            query_pane: QueryPane::new(candidates),
            schema_tree: SchemaTreePane::new(file_schemas),
            grid_pane: GridPane::new(),
            error_pane: ErrorPane::new(),
            open_prompt: None,
        }
    }

    /// Loads persisted query history (db-studio#42, from the XDG
    /// cache) into the query pane -- a separate builder step, not part
    /// of `new`, so tests constructing an `App` don't need a real
    /// cache directory or its own I/O.
    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.query_pane = self.query_pane.with_history(history);
        self
    }

    pub fn run(&mut self, terminal: &mut Tui) -> io::Result<()> {
        while self.running {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [query_area, middle_area, error_area, status_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [tree_area, grid_area] =
            Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(middle_area);
        self.query_pane
            .render(frame, query_area, self.focus == Focus::Query);
        self.schema_tree
            .render(frame, tree_area, self.focus == Focus::Tree);
        match &self.view {
            OutputView::Results => self.grid_pane.render(frame, grid_area),
            OutputView::Plan(rows) => plan_pane::render(frame, grid_area, rows),
            OutputView::Opcodes(sections) => opcode_pane::render(frame, grid_area, sections),
            OutputView::Stats => {
                let stats = self.files.get(self.active).map(|f| f.engine.stats());
                if let Some(stats) = stats {
                    stats_pane::render(frame, grid_area, stats);
                }
            }
        }
        if let Some(prompt) = &self.open_prompt {
            // Overlays the error pane's area rather than adding a new
            // layout row -- the two are never needed at once (typing a
            // path to open isn't something a query error interrupts).
            let text = format!("open file: {prompt}\u{2588}");
            let paragraph = ratatui::widgets::Paragraph::new(text)
                .style(ratatui::style::Style::default().fg(theme::text()))
                .block(theme::pane_block_borderless());
            frame.render_widget(paragraph, error_area);
        } else {
            self.error_pane.render(frame, error_area);
        }
        let active_file = self.files.get(self.active);
        let active_label = active_file.map(|f| file_label(&f.path)).unwrap_or_default();
        let active_mode = active_file
            .map(|f| f.engine.mode().to_string())
            .unwrap_or_default();
        status_bar::render(
            frame,
            status_area,
            &active_label,
            &active_mode,
            self.query_pane.cursor(),
            env!("CARGO_PKG_VERSION"),
        );
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
            if key.kind == KeyEventKind::Press {
                self.handle_key(key);
            }
        }
        Ok(())
    }

    /// One key event's worth of dispatch -- split out from
    /// `handle_events` so tests can drive it with a synthetic
    /// `KeyEvent` directly, without a real terminal for
    /// `crossterm::event::read` to poll.
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        let is_ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if is_ctrl_c {
            self.running = false;
            return;
        }
        // The open-file prompt (db-studio#42, `Ctrl+O`) claims every
        // key while active -- typing a path must not also move the
        // query cursor or the tree selection underneath it.
        if self.open_prompt.is_some() {
            self.handle_open_prompt_key(key);
            return;
        }
        // A query-pane completion popup claims Esc (close it) and Tab
        // (accept the selection) for itself before either reaches
        // this function's own quit/focus-cycle handling below.
        let query_has_popup = self.focus == Focus::Query && self.query_pane.has_open_popup();

        // `q` is a valid SQL character, so it can only double as
        // quit when the query pane isn't the one reading keys (#1's
        // scaffold had no text input yet, so it was unconditionally
        // safe there).
        let is_q_outside_query = self.focus != Focus::Query && key.code == KeyCode::Char('q');
        if is_q_outside_query || (key.code == KeyCode::Esc && !query_has_popup) {
            self.running = false;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.open_prompt = Some(String::new());
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            self.copy_current_view();
            return;
        }
        if key.code == KeyCode::Tab && !query_has_popup {
            self.focus = match self.focus {
                Focus::Query => Focus::Tree,
                Focus::Tree => Focus::Query,
            };
            return;
        }
        // F1-F4 switch the output view regardless of focus -- like
        // PageUp/PageDown, they're not text input the query pane
        // could otherwise claim.
        match key.code {
            KeyCode::F(1) => {
                if !self.run_tree_shortcut() {
                    self.view = OutputView::Results;
                }
                return;
            }
            KeyCode::F(2) => {
                self.refresh_plan();
                return;
            }
            KeyCode::F(3) => {
                self.refresh_opcodes();
                return;
            }
            KeyCode::F(4) => {
                self.view = OutputView::Stats;
                return;
            }
            _ => {}
        }
        // PageUp/PageDown scroll the grid regardless of focus -- the
        // grid itself isn't focusable, and nothing else claims these
        // keys.
        match key.code {
            KeyCode::PageDown => {
                self.grid_pane.scroll_down();
                return;
            }
            KeyCode::PageUp => {
                self.grid_pane.scroll_up();
                return;
            }
            _ => {}
        }
        // Shift+Left/Right scroll the grid horizontally (db-studio#42),
        // also regardless of focus -- plain Left/Right are already
        // claimed by the query pane's cursor and the tree's
        // collapse/expand, so a wide result set (e.g. `SELECT *` over
        // a log's Tier-3 columns) needs a key nothing else uses.
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            match key.code {
                KeyCode::Right => {
                    self.grid_pane.scroll_right();
                    return;
                }
                KeyCode::Left => {
                    self.grid_pane.scroll_left();
                    return;
                }
                _ => {}
            }
        }
        match self.focus {
            Focus::Query => {
                if let Some(query) = self.query_pane.handle_key(key) {
                    crate::history::append(&query);
                    self.submit(&query);
                }
            }
            Focus::Tree => {
                self.schema_tree.handle_key(key);
                // Enter/Space is schema_tree's own toggle-expand key
                // (handled above) -- if it landed on a file root
                // rather than a table/column, it also switches which
                // file queries run against (db-studio#18).
                let is_accept = matches!(key.code, KeyCode::Enter | KeyCode::Char(' '));
                if is_accept {
                    if let Some(file_key) = self.schema_tree.selected_file_key() {
                        self.switch_active_file(file_key.to_string());
                    }
                }
            }
        }
    }

    /// Makes the open file at `path` (matched against `OpenFile::path`'s
    /// display form, the same string the tree uses as its file-root
    /// key) the active query/completion target.
    fn switch_active_file(&mut self, path: String) {
        let Some(index) = self
            .files
            .iter()
            .position(|f| f.path.display().to_string() == path)
        else {
            return;
        };
        self.active = index;
        let candidates = self
            .files
            .get(index)
            .map(|f| completion::candidates(&f.engine.tables().unwrap_or_default()))
            .unwrap_or_default();
        self.query_pane.set_candidates(candidates);
    }

    /// `F1`'s object-browser shortcut (db-studio#42): a table selected
    /// in the schema tree runs `SELECT * FROM <table> LIMIT 1000`, a
    /// column runs `SELECT DISTINCT <column> FROM <table> LIMIT 100` --
    /// against whichever file it belongs to (switching to it first, so
    /// this works even when browsing a file that isn't already active).
    /// Returns `false` (nothing to do) when the tree's selection is a
    /// file root or nothing at all, so `F1` falls back to its plain
    /// "show Results" behavior.
    fn run_tree_shortcut(&mut self) -> bool {
        if let Some((file, table)) = self.schema_tree.selected_table() {
            let (file, table) = (file.to_string(), table.to_string());
            self.switch_active_file(file);
            let query = format!("SELECT * FROM {table} LIMIT 1000");
            self.query_pane.set_query(&query);
            self.submit(&query);
            return true;
        }
        if let Some((file, table, column)) = self.schema_tree.selected_column() {
            let (file, table, column) = (file.to_string(), table.to_string(), column.to_string());
            self.switch_active_file(file);
            let query = format!("SELECT DISTINCT {column} FROM {table} LIMIT 100");
            self.query_pane.set_query(&query);
            self.submit(&query);
            return true;
        }
        false
    }

    /// Copies whichever output view is currently showing to the system
    /// clipboard as plain text (db-studio#42, `Ctrl+Y`) -- no ANSI
    /// styling, no box-drawing border characters, regardless of which
    /// view (Results/Plan/Opcodes/Stats) is active.
    fn copy_current_view(&self) {
        let text = match &self.view {
            OutputView::Results => self.grid_pane.plain_text(),
            OutputView::Plan(rows) => plan_pane::plain_text(rows),
            OutputView::Opcodes(sections) => opcode_pane::plain_text(sections),
            OutputView::Stats => self
                .files
                .get(self.active)
                .map(|f| stats_pane::plain_text(&f.engine.stats()))
                .unwrap_or_default(),
        };
        clipboard::copy(&text);
    }

    /// Handles one key while the `Ctrl+O` open-file prompt is active
    /// (db-studio#42): `Enter` attempts to open the typed path,
    /// `Esc` cancels, `Backspace` edits, anything else appends.
    fn handle_open_prompt_key(&mut self, key: crossterm::event::KeyEvent) {
        let Some(prompt) = &mut self.open_prompt else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.open_prompt = None;
            }
            KeyCode::Enter => {
                let path = std::mem::take(prompt);
                self.open_prompt = None;
                self.open_file(path);
            }
            KeyCode::Backspace => {
                prompt.pop();
            }
            KeyCode::Char(c) => {
                prompt.push(c);
            }
            _ => {}
        }
    }

    /// Opens `path` and adds it as a new file root (db-studio#42) --
    /// the same per-extension dispatch `main.rs` uses for the files
    /// given on the command line, so a `.sqlite`/`.parquet`/`.log`
    /// opened this way behaves identically to one opened at startup.
    /// A bad path reports through the error pane rather than refusing
    /// to start, since the app is already running.
    fn open_file(&mut self, path: String) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return;
        }
        let path_buf = PathBuf::from(trimmed);
        match open_by_extension(&path_buf) {
            Ok(engine) => {
                let schema = FileSchema {
                    key: path_buf.display().to_string(),
                    label: file_label(&path_buf),
                    tables: engine.tables().unwrap_or_default(),
                };
                self.schema_tree.add_file(schema);
                self.files.push(OpenFile {
                    path: path_buf,
                    engine,
                });
                self.error_pane.clear();
            }
            Err(err) => self.error_pane.set_error(err.to_string()),
        }
    }

    fn submit(&mut self, query: &str) {
        // F5 means "run this and show me what happened" -- switching
        // back to Results here is what makes that visible. Without it,
        // running a query while viewing Plan/Opcodes/Stats updated the
        // grid invisibly behind whichever of those stayed on screen,
        // making F5 look like it had stopped working.
        self.view = OutputView::Results;
        let Some(file) = self.files.get_mut(self.active) else {
            return;
        };
        match file.engine.run_query(query) {
            Ok(result) => {
                let headers = result.columns;
                let rows = result
                    .rows
                    .into_iter()
                    .map(|row| row.iter().map(cell_display).collect())
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

    /// Recomputes the plan view from the query pane's *current* text
    /// (not the last-submitted query -- lets you preview a plan before
    /// running), routing a compile/parse failure to the same error pane
    /// `submit` uses rather than a separate error path.
    fn refresh_plan(&mut self) {
        let text = self.query_pane.text();
        let Some(file) = self.files.get(self.active) else {
            return;
        };
        match file.engine.explain_plan(&text) {
            Ok(rows) => {
                self.view = OutputView::Plan(rows);
                self.error_pane.clear();
            }
            Err(err) => self.error_pane.set_error(err.to_string()),
        }
    }

    /// Same as [`Self::refresh_plan`], for the opcode view.
    fn refresh_opcodes(&mut self) {
        let text = self.query_pane.text();
        let Some(file) = self.files.get(self.active) else {
            return;
        };
        match file.engine.explain_opcodes(&text) {
            Ok(sections) => {
                self.view = OutputView::Opcodes(sections);
                self.error_pane.clear();
            }
            Err(err) => self.error_pane.set_error(err.to_string()),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]
mod tests {
    use super::*;
    use db_core::engine::stream::StreamEngine;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn stream_fixture() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.log"
        ))
    }

    /// db-studio#37: a `.log` file opened through `StreamEngine` builds a
    /// real schema tree entry (not just an empty branch) and reports
    /// `Mode::Stream` in the status bar -- the same end-to-end wiring
    /// `App::new` already does for row/batch, verified here against a
    /// real engine rather than a fabricated `TableInfo`.
    #[test]
    fn a_log_file_gets_a_real_schema_tree_entry_and_stream_mode() {
        let path = stream_fixture();
        let engine = StreamEngine::open(&path).unwrap();
        let mut app = App::new(vec![OpenFile {
            path,
            engine: Box::new(engine),
        }]);

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        // Expand the file root, then the `log` table under it, so the
        // rendered tree shows the actual column list, not just the
        // collapsed file-root label -- `select_first`/`toggle_selected`
        // only take effect after a render has populated the tree's
        // flattened-item cache (see schema_tree.rs's own note).
        app.schema_tree.handle_key(enter());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.schema_tree.handle_key(down());
        app.schema_tree.handle_key(enter());
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let content = terminal.backend().buffer().content();
        let rendered: String = content.iter().map(|cell| cell.symbol()).collect();
        assert!(
            rendered.contains("sample.log"),
            "expected the file root in the tree:\n{rendered}"
        );
        assert!(
            rendered.contains("message"),
            "expected the `log` table's predefined columns in the tree:\n{rendered}"
        );
        assert!(
            rendered.contains("mode: stream"),
            "expected the status bar to report stream mode:\n{rendered}"
        );
    }

    fn enter() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::from(KeyCode::Enter)
    }

    fn down() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::from(KeyCode::Down)
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.query_pane
                .handle_key(crossterm::event::KeyEvent::from(KeyCode::Char(ch)));
        }
    }

    fn stream_app() -> App {
        let path = stream_fixture();
        let engine = StreamEngine::open(&path).unwrap();
        App::new(vec![OpenFile {
            path,
            engine: Box::new(engine),
        }])
    }

    fn rendered(app: &mut App) -> String {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// db-studio#38: the query-plan pane (F2) shows a real plan for a
    /// query against an open `.log` file -- `StreamEngine::explain_plan`
    /// already implements this; `refresh_plan` and `plan_pane::render`
    /// are mode-agnostic already, so this is a real-engine check, not
    /// new rendering code.
    #[test]
    fn f2_shows_a_real_plan_for_a_stream_query() {
        let mut app = stream_app();
        type_text(
            &mut app,
            "SELECT message FROM log WHERE severity_text = 'ERROR'",
        );
        app.refresh_plan();
        let text = rendered(&mut app);
        assert!(
            text.contains("log"),
            "expected the plan to mention the `log` table:\n{text}"
        );
    }

    /// db-studio#38: same as above, for the opcode-overview pane (F3).
    #[test]
    fn f3_shows_real_opcodes_for_a_stream_query() {
        let mut app = stream_app();
        type_text(
            &mut app,
            "SELECT message FROM log WHERE severity_text = 'ERROR'",
        );
        app.refresh_opcodes();
        let text = rendered(&mut app);
        assert!(
            !text.trim().is_empty(),
            "expected a non-empty opcode listing:\n{text}"
        );
    }

    /// db-studio#38: the file-statistics pane (F4) shows real
    /// `FileStats::Stream` data (bytes parsed / line count) -- the
    /// `stats_pane::render` match arm for it was written speculatively
    /// in M4 (db-studio#32) with no engine to produce it until now.
    #[test]
    fn f4_shows_real_stream_file_stats() {
        let mut app = stream_app();
        app.view = OutputView::Stats;
        let text = rendered(&mut app);
        assert!(
            text.contains("bytes parsed") && text.contains("lines"),
            "expected stream file stats to render:\n{text}"
        );
    }

    /// db-studio#39: a `JOIN` against a `.log` file's one table reports
    /// a specific, readable message in the error pane -- `submit`
    /// already routes any `EngineError` through `err.to_string()` with
    /// no per-`ErrorKind` branching, so this is a real-message check,
    /// not new error-handling code.
    #[test]
    fn a_join_against_a_stream_file_shows_a_specific_error() {
        let mut app = stream_app();
        app.submit("SELECT log.message FROM log JOIN log AS l2 ON log.message = l2.message");
        let text = rendered(&mut app);
        assert!(
            text.contains("JOIN") && text.contains("one table"),
            "expected a specific unsupported-JOIN message, not a generic one:\n{text}"
        );
    }

    /// db-studio#39: same as above, for a window function.
    #[test]
    fn a_window_function_against_a_stream_file_shows_a_specific_error() {
        let mut app = stream_app();
        app.submit("SELECT message, ROW_NUMBER() OVER () FROM log");
        let text = rendered(&mut app);
        assert!(
            text.contains("window function"),
            "expected a specific unsupported-window-function message:\n{text}"
        );
    }

    /// db-studio#40 (M5's "wire together" ticket, mirroring #27's
    /// equivalent for M3): all three modes open in one session,
    /// switching between them via the schema tree (db-studio#18) runs
    /// each query against the right engine and reports the right mode
    /// in the status bar -- exactly what already works for row<->batch,
    /// now proven for row<->batch<->stream together.
    #[test]
    fn all_three_modes_open_together_and_switch_correctly() {
        use db_core::engine::column::BatchEngine;
        use db_core::engine::row::RowEngine;

        let sqlite_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.sqlite"
        ));
        let parquet_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.parquet"
        ));
        let log_path = stream_fixture();

        let mut app = App::new(vec![
            OpenFile {
                engine: Box::new(RowEngine::open(&sqlite_path).unwrap()),
                path: sqlite_path.clone(),
            },
            OpenFile {
                engine: Box::new(BatchEngine::open(&parquet_path).unwrap()),
                path: parquet_path.clone(),
            },
            OpenFile {
                engine: Box::new(StreamEngine::open(&log_path).unwrap()),
                path: log_path.clone(),
            },
        ]);

        // sqlite is active by default (App::new's initial `active: 0`).
        app.submit("SELECT name, price FROM items ORDER BY id");
        let text = rendered(&mut app);
        assert!(text.contains("mode: row"), "expected row mode:\n{text}");
        assert!(
            text.contains("widget"),
            "expected real row-mode results:\n{text}"
        );

        app.switch_active_file(parquet_path.display().to_string());
        app.submit("SELECT region FROM sample");
        let text = rendered(&mut app);
        assert!(text.contains("mode: batch"), "expected batch mode:\n{text}");
        assert!(
            text.contains("south"),
            "expected real batch-mode results:\n{text}"
        );

        app.switch_active_file(log_path.display().to_string());
        app.submit("SELECT message FROM log WHERE severity_text = 'ERROR'");
        let text = rendered(&mut app);
        assert!(
            text.contains("mode: stream"),
            "expected stream mode:\n{text}"
        );
        assert!(
            text.contains("GET /api/orders 500"),
            "expected real stream-mode results:\n{text}"
        );
    }

    /// db-studio#42/#43: `F1` on a table selected in the schema tree
    /// runs `SELECT * FROM <table> LIMIT 1000` and shows its results,
    /// rather than just switching to the (until-now-empty) Results view.
    #[test]
    fn f1_on_a_selected_table_runs_a_select_star_and_shows_results() {
        let mut app = stream_app();
        rendered(&mut app); // populates the tree's flattened-item cache
        app.schema_tree.handle_key(enter()); // expand the file root
        rendered(&mut app); // populates the cache for the now-visible table
        app.schema_tree.handle_key(down()); // select the `log` table

        assert_eq!(
            app.schema_tree.selected_table().map(|(_, t)| t),
            Some("log")
        );
        assert!(app.run_tree_shortcut());
        assert_eq!(app.query_pane.text(), "SELECT * FROM log LIMIT 1000");
        let text = rendered(&mut app);
        assert!(
            text.contains("message") && text.contains("row(s)"),
            "expected F1's SELECT * to have actually run:\n{text}"
        );
    }

    /// db-studio#42: pressing `q` while the tree has focus quits --
    /// `q` still can't double as quit while the query editor has focus,
    /// since it's a valid SQL character there.
    #[test]
    fn q_quits_when_the_tree_has_focus_but_not_the_query_pane() {
        let mut app = stream_app();
        app.focus = Focus::Tree;
        app.handle_key(crossterm::event::KeyEvent::from(KeyCode::Char('q')));
        assert!(!app.running);

        let mut app2 = stream_app();
        app2.focus = Focus::Query;
        app2.handle_key(crossterm::event::KeyEvent::from(KeyCode::Char('q')));
        assert!(app2.running);
        assert_eq!(app2.query_pane.text(), "q");
    }

    /// db-studio#42: `Ctrl+O` opens the prompt, typing a path and
    /// pressing Enter opens it as a new file root -- a bad path reports
    /// through the error pane instead of crashing or being ignored.
    #[test]
    fn open_file_adds_a_new_root_and_a_bad_path_is_a_visible_error() {
        let mut app = stream_app();
        let sqlite_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.sqlite"
        ));
        let before = app.files.len();
        app.open_file(sqlite_path.display().to_string());
        assert_eq!(app.files.len(), before + 1);

        app.open_file("/no/such/file.sqlite".to_string());
        assert_eq!(app.files.len(), before + 1, "a bad path must not open");
    }

    /// db-studio#44: `Cell::Null` and a real empty string must render
    /// differently -- otherwise a sparse result (e.g. `SELECT *` over a
    /// `.log` file's Tier-3 columns, where most fields don't apply to
    /// most rows) looks indistinguishable from one full of blanks.
    #[test]
    fn cell_display_marks_null_but_leaves_a_real_empty_string_blank() {
        assert_eq!(cell_display(&Cell::Null), "<NULL>");
        assert_eq!(cell_display(&Cell::Text(String::new())), "");
        assert_eq!(cell_display(&Cell::Text("hi".to_string())), "hi");
        assert_eq!(cell_display(&Cell::Int(0)), "0");
    }

    /// Same check end to end: a real stream query over a Tier-3 field
    /// that only some lines carry shows `<NULL>` for the rows missing
    /// it, not a blank cell -- exactly the sparse-`SELECT *` shape
    /// db-studio#44 was filed over.
    #[test]
    fn a_real_null_column_shows_the_null_marker_in_the_grid() {
        let path = std::env::temp_dir().join(format!(
            "db_studio_null_marker_test_{}.log",
            std::process::id()
        ));
        std::fs::write(&path, "ts=2026-01-01T00:00:00Z level=info msg=\"has extra\" extra=1\nts=2026-01-01T00:00:01Z level=info msg=\"no extra\"\n").unwrap();

        let engine = StreamEngine::open(&path).unwrap();
        let mut app = App::new(vec![OpenFile {
            path,
            engine: Box::new(engine),
        }]);
        app.submit("SELECT extra FROM log");
        let text = rendered(&mut app);
        assert!(
            text.contains("<NULL>"),
            "expected the NULL marker for the row missing `extra`:\n{text}"
        );
    }
}
