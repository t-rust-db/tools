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
}

impl QueryPane {
    pub fn new(candidates: Vec<String>) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_selection_style(Style::default().bg(theme::selection_bg()));
        Self {
            textarea,
            candidates,
            popup: None,
        }
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
            self.textarea = TextArea::default();
            self.textarea
                .set_selection_style(Style::default().bg(theme::selection_bg()));
            self.popup = None;
            return Some(text);
        }

        if self.popup.is_some() && self.handle_popup_key(key.code) {
            return None;
        }

        self.textarea.input(key);
        self.update_popup();
        None
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
        // The submit key isn't discoverable otherwise -- Enter inserting
        // a newline instead of running the query (needed for multi-line
        // editing) reads as "the engine stopped working" without this.
        self.textarea
            .set_block(theme::pane_block("query -- F5 to run", focused));
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
    fn typing_then_f5_submits_and_clears() {
        let mut pane = pane_with_candidates();
        for c in "SELECT 1".chars() {
            assert_eq!(pane.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(pane.handle_key(f5()), Some("SELECT 1".to_string()));
        assert_eq!(pane.textarea.lines(), &[""]);
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
}
