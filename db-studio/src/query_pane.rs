//! The top pane (db-studio#9): a `tui-textarea-2`-backed multi-line SQL
//! editor with syntax highlighting from `db-core`'s own tokenizer (see
//! `highlight.rs`), plus a schema-aware completion popup (db-studio#11).
//! Enter inserts a newline; `F5` submits -- `QueryPane::new`'s
//! single-line hand-rolled cursor logic is gone.
//!
//! `F5`, not `Ctrl+Enter`: a plain terminal (no Kitty keyboard protocol --
//! macOS Terminal.app and a generic xterm included) cannot distinguish
//! `Ctrl+Enter` from plain `Enter` at the byte level, both send the same
//! carriage return, so that binding would be unreachable on most real
//! terminals. A function key has its own distinct escape sequence
//! everywhere and isn't shadowed by any of `tui-textarea-2`'s own
//! bindings (unlike e.g. `Ctrl+R`, which is its `redo`).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState};
use ratatui::Frame;
use tui_textarea::TextArea;

use crate::completion;
use crate::highlight;
use crate::theme;

const COMPLETION_LIMIT: usize = 8;

struct Popup {
    matches: Vec<String>,
    selected: usize,
}

pub struct QueryPane {
    textarea: TextArea<'static>,
    candidates: Vec<String>,
    popup: Option<Popup>,
    /// Previously submitted queries, oldest first (db-studio#42) --
    /// `Up`/`Down` at the buffer's top/bottom row cycle through these
    /// like a shell's history, rather than moving the cursor past the
    /// edge of the text (nothing else claims that gesture there).
    history: Vec<String>,
    /// Index into `history` while navigating it; `None` means the
    /// buffer holds live (or not-yet-submitted) text, not a recalled
    /// entry.
    history_index: Option<usize>,
    /// The buffer's text from just before `Up` first stepped into
    /// history -- restored verbatim if `Down` steps back past the
    /// newest entry, so navigating history and returning loses nothing
    /// you'd already typed.
    draft: Option<String>,
}

impl QueryPane {
    pub fn new(candidates: Vec<String>) -> Self {
        Self {
            textarea: Self::configure(TextArea::default()),
            candidates,
            popup: None,
            history: Vec::new(),
            history_index: None,
            draft: None,
        }
    }

    /// Loads previously submitted queries (db-studio#42, from the XDG
    /// cache) for `Up`/`Down` history navigation -- oldest first, same
    /// order [`history`] is appended to on submit.
    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.history = history;
        self
    }

    fn configure(mut textarea: TextArea<'static>) -> TextArea<'static> {
        textarea.set_selection_style(Style::default().bg(theme::selection_bg()));
        // A gutter, not a decoration: knowing which line an error or a
        // plan detail refers to needs line numbers to point at -- copy
        // (db-studio#42's clipboard yank) reads the buffer's raw text,
        // never the gutter, so pasting elsewhere never carries them.
        textarea.set_line_number_style(Style::default().fg(theme::subtext()));
        textarea
    }

    /// Replaces the buffer's text outright -- the schema tree's
    /// table/column shortcuts (db-studio#42) and history navigation
    /// both need this rather than simulated keystrokes.
    fn set_text(&mut self, text: &str) {
        self.textarea = Self::configure(TextArea::from(text.split('\n').map(str::to_string)));
        self.textarea.move_cursor(tui_textarea::CursorMove::Bottom);
        self.textarea.move_cursor(tui_textarea::CursorMove::End);
        self.popup = None;
    }

    /// Same as [`Self::set_text`], for the schema tree's table/column
    /// shortcuts -- the only external caller, so this stays a thin
    /// public wrapper rather than making `set_text` itself public.
    pub fn set_query(&mut self, text: &str) {
        self.set_text(text);
    }

    /// Replaces the completion candidate list -- db-studio#18 calls this
    /// when the active file changes, so completion offers the newly
    /// active file's names, not whichever file was active at startup.
    /// Closes any open popup: its matches were ranked against the old
    /// candidate set and no longer mean anything.
    pub fn set_candidates(&mut self, candidates: Vec<String>) {
        self.candidates = candidates;
        self.popup = None;
    }

    /// Whether a completion popup is open -- the app checks this before
    /// treating `Esc` as quit (closes the popup instead) or `Tab` as a
    /// focus-cycle key (accepts the completion instead).
    pub fn has_open_popup(&self) -> bool {
        self.popup.is_some()
    }

    /// The cursor's (row, col), 0-based -- db-studio#19's status bar
    /// renders this 1-based, the way editors conventionally do.
    pub fn cursor(&self) -> (usize, usize) {
        self.textarea.cursor()
    }

    /// The current buffer text -- db-studio#30/#31's Plan/Opcodes views
    /// read this (not the last-submitted query) so they can preview a
    /// query before running it with `F5`.
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Handles one key event. Returns the submitted query text on `F5`
    /// (and clears the buffer), otherwise `None`. A popup consumes
    /// Up/Down/Tab/Enter/Esc itself before any of them reach the
    /// textarea or (for Esc) the app's quit handling.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.code == KeyCode::F(5) {
            let text = self.textarea.lines().join("\n");
            if text.trim().is_empty() {
                return None;
            }
            // Deliberately *not* cleared: F2/F3 (Plan/Opcodes) read this
            // same buffer to preview a query before running it, and
            // clearing it here left them with nothing right after F5 --
            // "parse: empty statement" on the very query you just ran.
            // Every other SQL tool keeps the query visible after
            // running it too, for the same reason: you're usually about
            // to tweak and re-run it, not start from scratch.
            self.push_history(text.clone());
            return Some(text);
        }

        if self.popup.is_some() && self.handle_popup_key(key.code) {
            return None;
        }

        // The popup branch above already returned if one was open, so
        // reaching here means there isn't one.
        if self.handle_history_key(key.code) {
            return None;
        }

        self.textarea.input(key);
        self.history_index = None;
        self.update_popup();
        None
    }

    /// Appends `text` to history (db-studio#42), deduping an immediate
    /// repeat of the last entry -- re-running the same query with `F5`
    /// shouldn't fill history with copies of itself.
    fn push_history(&mut self, text: String) {
        if self.history.last() != Some(&text) {
            self.history.push(text);
        }
        self.history_index = None;
        self.draft = None;
    }

    /// `Up` at the buffer's first line steps to an older history entry;
    /// `Down` at its last line steps to a newer one (or back to the
    /// live draft past the newest). Returns `true` when the key was
    /// consumed this way, so the caller doesn't also forward it to the
    /// textarea as a cursor move.
    fn handle_history_key(&mut self, code: KeyCode) -> bool {
        let (row, _) = self.textarea.cursor();
        let last_row = self.textarea.lines().len().saturating_sub(1);
        match code {
            KeyCode::Up if row == 0 && !self.history.is_empty() => {
                let next_index = match self.history_index {
                    None => {
                        self.draft = Some(self.textarea.lines().join("\n"));
                        self.history.len().saturating_sub(1)
                    }
                    Some(i) => i.saturating_sub(1),
                };
                self.history_index = Some(next_index);
                let text = self.history.get(next_index).cloned().unwrap_or_default();
                self.set_text(&text);
                true
            }
            KeyCode::Down if row == last_row && self.history_index.is_some() => {
                match self.history_index {
                    Some(i) if i.saturating_add(1) < self.history.len() => {
                        let next_index = i + 1;
                        self.history_index = Some(next_index);
                        let text = self.history.get(next_index).cloned().unwrap_or_default();
                        self.set_text(&text);
                    }
                    _ => {
                        self.history_index = None;
                        let text = self.draft.take().unwrap_or_default();
                        self.set_text(&text);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Returns `true` when `code` was a popup-navigation/accept/dismiss
    /// key and has been fully handled (the caller must not also forward
    /// it to the textarea or treat `Esc` as quit).
    fn handle_popup_key(&mut self, code: KeyCode) -> bool {
        let Some(popup) = &mut self.popup else {
            return false;
        };
        match code {
            KeyCode::Down => {
                popup.selected = (popup.selected + 1) % popup.matches.len();
                true
            }
            KeyCode::Up => {
                popup.selected = popup
                    .selected
                    .checked_sub(1)
                    .unwrap_or(popup.matches.len() - 1);
                true
            }
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(chosen) = popup.matches.get(popup.selected).cloned() {
                    self.accept_completion(&chosen);
                }
                true
            }
            KeyCode::Esc => {
                self.popup = None;
                true
            }
            _ => false,
        }
    }

    fn current_line(&self, row: usize) -> String {
        self.textarea.lines().get(row).cloned().unwrap_or_default()
    }

    fn accept_completion(&mut self, chosen: &str) {
        let (row, col) = self.textarea.cursor();
        let word_start = current_word_start(&self.current_line(row), col);
        self.textarea.move_cursor(tui_textarea::CursorMove::Jump(
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(word_start).unwrap_or(u16::MAX),
        ));
        self.textarea.delete_str(col.saturating_sub(word_start));
        self.textarea.insert_str(chosen);
        self.popup = None;
    }

    /// Recomputes the completion popup from the word ending at the
    /// cursor, or clears it: an empty partial word, a cursor inside a
    /// string/blob literal (db-studio#11's "no popup mid-string" rule),
    /// or no fuzzy matches all mean no popup.
    fn update_popup(&mut self) {
        let (row, col) = self.textarea.cursor();
        let text = self.textarea.lines().join("\n");
        if highlight::is_inside_string_or_blob(&text, row, col) {
            self.popup = None;
            return;
        }
        let line = self.current_line(row);
        let word_start = current_word_start(&line, col);
        let partial: String = line
            .chars()
            .skip(word_start)
            .take(col - word_start)
            .collect();
        let matches = completion::rank(&self.candidates, &partial, COMPLETION_LIMIT);
        self.popup = if matches.is_empty() {
            None
        } else {
            Some(Popup {
                matches,
                selected: 0,
            })
        };
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        self.textarea.clear_custom_highlight();
        let text = self.textarea.lines().join("\n");
        for (start, end, style) in highlight::highlights(&text) {
            self.textarea.custom_highlight((start, end), style, 10);
        }
        // Borderless (db-studio#42): a border/title on a pane visited
        // on every keystroke read as chrome, not information, once the
        // line-number gutter already marks its left edge.
        let _ = focused;
        self.textarea.set_block(theme::pane_block_borderless());
        frame.render_widget(&self.textarea, area);
    }

    /// Renders the completion popup, if open. Ratatui has no z-ordering
    /// -- a later `render_widget` call simply overwrites whatever cells
    /// it touches -- so the caller must call this *after* every other
    /// pane, or the schema tree/grid panes paint over the popup instead
    /// of the other way around.
    pub fn render_popup(&self, frame: &mut Frame, query_area: Rect) {
        if let Some(popup) = &self.popup {
            render_popup(frame, query_area, popup);
        }
    }
}

/// The char index (into `line`) where the identifier ending at column
/// `col` began -- `col` itself when there's no partial word to complete.
fn current_word_start(line: &str, col: usize) -> usize {
    let prefix: Vec<char> = line.chars().take(col).collect();
    let mut start = prefix.len();
    while let Some(c) = start.checked_sub(1).and_then(|i| prefix.get(i)) {
        if c.is_alphanumeric() || *c == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

fn render_popup(frame: &mut Frame, query_area: Rect, popup: &Popup) {
    let height = u16::try_from(popup.matches.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let popup_area = Rect {
        x: query_area.x + 2,
        y: query_area.y + query_area.height,
        width: 30.min(query_area.width),
        height: height.min(query_area.height.saturating_add(6)),
    };
    let items: Vec<ListItem> = popup
        .matches
        .iter()
        .map(|m| ListItem::new(m.as_str()))
        .collect();
    let mut state = ListState::default();
    state.select(Some(popup.selected));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(Style::default().bg(theme::base()).fg(theme::text())),
        )
        .highlight_style(
            Style::default()
                .bg(theme::selection_bg())
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, popup_area, &mut state);
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn f5() -> KeyEvent {
        KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)
    }

    fn pane_with_candidates() -> QueryPane {
        QueryPane::new(vec!["items".to_string(), "price".to_string()])
    }

    #[test]
    fn typing_then_f5_submits_and_keeps_the_text() {
        let mut pane = pane_with_candidates();
        for c in "SELECT 1".chars() {
            assert_eq!(pane.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(pane.handle_key(f5()), Some("SELECT 1".to_string()));
        // Not cleared: F2/F3 preview a plan/opcodes from this same
        // buffer, and pressing F5 again should re-run the same query,
        // not an empty one.
        assert_eq!(pane.textarea.lines(), &["SELECT 1"]);
    }

    #[test]
    fn f5_twice_in_a_row_submits_the_same_query_both_times() {
        let mut pane = pane_with_candidates();
        for c in "SELECT 1".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(pane.handle_key(f5()), Some("SELECT 1".to_string()));
        assert_eq!(pane.handle_key(f5()), Some("SELECT 1".to_string()));
    }

    #[test]
    fn plain_enter_inserts_a_newline_rather_than_submitting_when_no_popup() {
        let mut pane = QueryPane::new(vec![]);
        pane.handle_key(key(KeyCode::Char('a')));
        assert_eq!(pane.handle_key(key(KeyCode::Enter)), None);
        pane.handle_key(key(KeyCode::Char('b')));
        assert_eq!(pane.textarea.lines(), &["a", "b"]);
    }

    #[test]
    fn f5_on_blank_input_does_not_submit() {
        let mut pane = pane_with_candidates();
        assert_eq!(pane.handle_key(f5()), None);
    }

    #[test]
    fn f5_on_whitespace_only_input_does_not_submit() {
        let mut pane = pane_with_candidates();
        pane.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(pane.handle_key(f5()), None);
    }

    #[test]
    fn typing_a_partial_identifier_opens_a_popup_with_matches() {
        let mut pane = pane_with_candidates();
        for c in "pri".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        assert!(pane.popup.is_some());
        assert_eq!(pane.popup.as_ref().unwrap().matches, vec!["price"]);
    }

    #[test]
    fn tab_accepts_the_selected_completion_replacing_the_partial_word() {
        let mut pane = pane_with_candidates();
        for c in "pri".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        pane.handle_key(key(KeyCode::Tab));
        assert_eq!(pane.textarea.lines(), &["price"]);
        assert!(pane.popup.is_none());
    }

    #[test]
    fn esc_closes_the_popup_and_reports_it_wanted_esc() {
        let mut pane = pane_with_candidates();
        for c in "pri".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        assert!(pane.has_open_popup());
        pane.handle_key(key(KeyCode::Esc));
        assert!(pane.popup.is_none());
        assert!(!pane.has_open_popup());
    }

    #[test]
    fn no_popup_inside_a_string_literal() {
        let mut pane = pane_with_candidates();
        for c in "'pri".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        assert!(pane.popup.is_none());
    }

    #[test]
    fn no_popup_when_the_partial_word_is_empty() {
        let mut pane = pane_with_candidates();
        pane.handle_key(key(KeyCode::Char(' ')));
        assert!(pane.popup.is_none());
    }

    #[test]
    fn down_then_tab_accepts_the_second_match() {
        let mut pane = QueryPane::new(vec!["price".to_string(), "priceless".to_string()]);
        for c in "pri".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        pane.handle_key(key(KeyCode::Down));
        pane.handle_key(key(KeyCode::Tab));
        assert_eq!(pane.textarea.lines(), &["priceless"]);
    }

    #[test]
    fn up_at_the_top_line_recalls_the_most_recent_history_entry() {
        let mut pane = pane_with_candidates()
            .with_history(vec!["SELECT 1".to_string(), "SELECT 2".to_string()]);
        pane.handle_key(key(KeyCode::Up));
        assert_eq!(pane.text(), "SELECT 2");
        pane.handle_key(key(KeyCode::Up));
        assert_eq!(pane.text(), "SELECT 1");
        // Oldest entry reached -- Up again stays put, doesn't panic or wrap.
        pane.handle_key(key(KeyCode::Up));
        assert_eq!(pane.text(), "SELECT 1");
    }

    #[test]
    fn down_past_the_newest_entry_restores_the_live_draft() {
        let mut pane = pane_with_candidates().with_history(vec!["SELECT 1".to_string()]);
        for c in "SELECT 2".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        pane.handle_key(key(KeyCode::Up));
        assert_eq!(pane.text(), "SELECT 1");
        pane.handle_key(key(KeyCode::Down));
        assert_eq!(pane.text(), "SELECT 2");
    }

    #[test]
    fn submitting_a_query_appends_it_to_history() {
        let mut pane = pane_with_candidates();
        for c in "SELECT 1".chars() {
            pane.handle_key(key(KeyCode::Char(c)));
        }
        pane.handle_key(f5());
        assert_eq!(pane.history, vec!["SELECT 1".to_string()]);
    }

    #[test]
    fn set_query_replaces_the_buffer_and_moves_the_cursor_to_the_end() {
        let mut pane = pane_with_candidates();
        pane.set_query("SELECT * FROM t");
        assert_eq!(pane.text(), "SELECT * FROM t");
        assert_eq!(pane.cursor(), (0, "SELECT * FROM t".len()));
    }
}
