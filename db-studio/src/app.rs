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
use db_core::engine::Engine;
use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::completion;
use crate::error_pane::ErrorPane;
use crate::grid_pane::{Grid, GridPane};
use crate::query_pane::QueryPane;
use crate::schema_tree::{FileSchema, SchemaTreePane};
use crate::terminal::Tui;
use crate::{opcode_pane, plan_pane, stats_pane, status_bar};
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
        self.error_pane.render(frame, error_area);
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
            // F1-F4 switch the output view regardless of focus -- like
            // PageUp/PageDown, they're not text input the query pane
            // could otherwise claim.
            match key.code {
                KeyCode::F(1) => {
                    self.view = OutputView::Results;
                    return Ok(());
                }
                KeyCode::F(2) => {
                    self.refresh_plan();
                    return Ok(());
                }
                KeyCode::F(3) => {
                    self.refresh_opcodes();
                    return Ok(());
                }
                KeyCode::F(4) => {
                    self.view = OutputView::Stats;
                    return Ok(());
                }
                _ => {}
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
        Ok(())
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
}
