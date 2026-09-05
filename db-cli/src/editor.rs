//! Line editing: cursor movement, insert/delete, history navigation, and
//! raw-terminal key reading. No dependency on any particular engine's types.

use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::history::History;

#[derive(Debug)]
pub enum ReadlineError {
    Eof,
    Interrupted,
    Io(io::Error),
}

impl std::fmt::Display for ReadlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadlineError::Eof => write!(f, "EOF"),
            ReadlineError::Interrupted => write!(f, "interrupted"),
            ReadlineError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Colorizes a line of input for interactive display. The default,
/// keyword-list-based implementation (`highlight::highlight`, unchanged
/// from before this hook existed) stays column-rs's/any engine's default;
/// an engine with its own tokenizer plugs a richer one in via
/// [`Readline::set_highlighter`] (#6).
///
/// **Invariant:** `highlight(line)`'s return value must have the exact
/// same *visible* character count as `line` -- `redraw_at`'s cursor-column
/// math is computed from `line`'s raw char count and assumes the printed
/// `display` string lines up with it one-for-one. ANSI escape codes are
/// zero-width and fine to add; anything that inserts or removes visible
/// characters (e.g. wrapping the line in brackets) will make the cursor
/// land in the wrong column.
pub trait Highlighter {
    fn highlight(&self, line: &str) -> String;
}

struct DefaultHighlighter;

impl Highlighter for DefaultHighlighter {
    fn highlight(&self, line: &str) -> String {
        highlight::highlight(line)
    }
}

/// Tab-completion hook (#6): given the full `line` and the cursor's char
/// position within it, returns `(prefix_start, candidates)` -- the char
/// index where the word being completed begins (so the caller replaces
/// `line[prefix_start..cursor]`) and the full replacement candidates for
/// that prefix. An empty `candidates` list means "no completions". The
/// default is a no-op; an engine supplies its own keyword/table/column
/// list via [`Readline::set_completer`].
pub trait Completer {
    fn complete(&self, line: &str, cursor: usize) -> (usize, Vec<String>);
}

struct NoCompleter;

impl Completer for NoCompleter {
    fn complete(&self, _line: &str, cursor: usize) -> (usize, Vec<String>) {
        (cursor, Vec::new())
    }
}

pub struct Readline {
    history: History,
    tty: bool,
    color: bool,
    highlighter: Box<dyn Highlighter>,
    completer: Box<dyn Completer>,
    /// Physical row (0-indexed from the top row of the current line's
    /// rendering) the cursor was left on after the last redraw -- how many
    /// rows `redraw_at` must move up before reprinting.
    prev_cursor_row: usize,
}

impl Default for Readline {
    fn default() -> Self {
        Self::new()
    }
}

impl Readline {
    pub fn new() -> Self {
        Readline {
            history: History::new(),
            tty: is_tty(),
            color: true,
            highlighter: Box::new(DefaultHighlighter),
            completer: Box::new(NoCompleter),
            prev_cursor_row: 0,
        }
    }

    pub fn set_color(&mut self, enabled: bool) {
        self.color = enabled;
    }

    /// Plug in an engine-specific [`Highlighter`]. Default behavior
    /// (`highlight::highlight`'s built-in keyword list) is unchanged until
    /// this is called.
    pub fn set_highlighter(&mut self, highlighter: impl Highlighter + 'static) {
        self.highlighter = Box::new(highlighter);
    }

    /// Plug in an engine-specific [`Completer`]. Tab does nothing until
    /// this is called (the default [`NoCompleter`] always returns no
    /// candidates).
    pub fn set_completer(&mut self, completer: impl Completer + 'static) {
        self.completer = Box::new(completer);
    }

    pub fn load_history(&mut self, path: &Path) {
        self.history.load(path);
    }

    pub fn save_history(&self, path: &Path) {
        self.history.save(path);
    }

    pub fn add_history_entry(&mut self, line: &str) {
        self.history.add(line);
    }

    pub fn read_line(&mut self, prompt: &str) -> Result<String, ReadlineError> {
        if !self.tty {
            // Piped input: plain line read.
            let stdin = io::stdin();
            let mut line = String::new();
            return match stdin.lock().read_line(&mut line) {
                Ok(0) => Err(ReadlineError::Eof),
                Ok(_) => Ok(line.trim_end_matches('\n').to_string()),
                Err(e) => Err(ReadlineError::Io(e)),
            };
        }

        let mut stdout = io::stdout();
        let stdin = io::stdin();

        print!("{prompt}");
        stdout.flush().ok();

        let mut line = String::new();
        let mut hist_idx: Option<usize> = None;
        let mut cursor: usize = 0;
        self.prev_cursor_row = 0;

        // Enable raw mode for key-by-key input.
        let _raw = match term::RawMode::enable() {
            Some(r) => r,
            None => {
                // Fallback to plain read when raw mode is unavailable.
                return match stdin.lock().read_line(&mut line) {
                    Ok(0) => Err(ReadlineError::Eof),
                    Ok(_) => Ok(line.trim_end_matches('\n').to_string()),
                    Err(e) => Err(ReadlineError::Io(e)),
                };
            }
        };

        loop {
            let key = match term::read_key() {
                Ok(k) => k,
                Err(_) => continue,
            };

            match key {
                term::Key::Char('\n') | term::Key::Char('\r') => {
                    self.finish_line(prompt, &line);
                    return Ok(line);
                }
                term::Key::Char('\x03') => {
                    // Ctrl-C
                    self.finish_line(prompt, &line);
                    return Err(ReadlineError::Interrupted);
                }
                term::Key::Char('\x04') => {
                    // Ctrl-D
                    if line.is_empty() {
                        self.finish_line(prompt, &line);
                        return Err(ReadlineError::Eof);
                    }
                }
                term::Key::Char('\x7f') | term::Key::Backspace => {
                    if cursor > 0 {
                        let idx = char_byte_index(&line, cursor - 1);
                        line.remove(idx);
                        cursor -= 1;
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                term::Key::Delete => {
                    let len = line.chars().count();
                    if cursor < len {
                        let idx = char_byte_index(&line, cursor);
                        line.remove(idx);
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                // Ctrl-A / Ctrl-E: emacs/readline aliases for Home/End.
                term::Key::Char('\x01') => {
                    cursor = 0;
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                term::Key::Char('\x05') => {
                    cursor = line.chars().count();
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                // Ctrl-B / Ctrl-F: emacs/readline aliases for Left/Right.
                term::Key::Char('\x02') => {
                    if cursor > 0 {
                        cursor -= 1;
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                term::Key::Char('\x06') => {
                    if cursor < line.chars().count() {
                        cursor += 1;
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                // Ctrl-K: kill from the cursor to end of line.
                term::Key::Char('\x0b') => {
                    let idx = char_byte_index(&line, cursor);
                    line.truncate(idx);
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                // Ctrl-U: kill from the cursor to start of line.
                term::Key::Char('\x15') => {
                    let idx = char_byte_index(&line, cursor);
                    line.drain(..idx);
                    cursor = 0;
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                // Ctrl-W: kill the previous word.
                term::Key::Char('\x17') => {
                    let start = prev_word_boundary(&line, cursor);
                    let start_idx = char_byte_index(&line, start);
                    let cursor_idx = char_byte_index(&line, cursor);
                    line.drain(start_idx..cursor_idx);
                    cursor = start;
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                // Alt-B / Alt-F: word left / right.
                term::Key::Alt('b') => {
                    cursor = prev_word_boundary(&line, cursor);
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                term::Key::Alt('f') => {
                    cursor = next_word_boundary(&line, cursor);
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                // Ctrl-L: clear screen, then redraw at the top.
                term::Key::Char('\x0c') => {
                    print!("\x1b[2J\x1b[H");
                    self.prev_cursor_row = 0;
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                // Tab: complete the word at the cursor via `self.completer`.
                // One candidate inserts it outright; multiple candidates
                // insert their longest common prefix (classic readline
                // "partial completion") if it extends past what's already
                // typed. No completer set (or zero candidates) does
                // nothing.
                term::Key::Char('\t') => {
                    let (prefix_start, candidates) = self.completer.complete(&line, cursor);
                    let replacement = match candidates.len() {
                        0 => None,
                        1 => Some(candidates[0].clone()),
                        _ => {
                            let lcp = longest_common_prefix(&candidates);
                            let prefix_len = cursor.saturating_sub(prefix_start);
                            (lcp.chars().count() > prefix_len).then_some(lcp)
                        }
                    };
                    if let Some(replacement) = replacement {
                        let start_idx = char_byte_index(&line, prefix_start);
                        let cursor_idx = char_byte_index(&line, cursor);
                        line.replace_range(start_idx..cursor_idx, &replacement);
                        cursor = prefix_start + replacement.chars().count();
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                // Ctrl-P / Ctrl-N: emacs/readline aliases for history Up/Down.
                term::Key::Char('\x10') => {
                    if let Some(prev) = self.history.prev(&mut hist_idx) {
                        line = prev.to_string();
                        cursor = line.chars().count();
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                term::Key::Char('\x0e') => {
                    if let Some(next) = self.history.next(&mut hist_idx) {
                        line = next.to_string();
                    } else {
                        hist_idx = None;
                        line.clear();
                    }
                    cursor = line.chars().count();
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                term::Key::Char(c) if !c.is_control() => {
                    let idx = char_byte_index(&line, cursor);
                    line.insert(idx, c);
                    cursor += 1;
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                term::Key::Left => {
                    if cursor > 0 {
                        cursor -= 1;
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                term::Key::Right => {
                    if cursor < line.chars().count() {
                        cursor += 1;
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                term::Key::Home => {
                    cursor = 0;
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                term::Key::End => {
                    cursor = line.chars().count();
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                term::Key::Up => {
                    if let Some(prev) = self.history.prev(&mut hist_idx) {
                        line = prev.to_string();
                        cursor = line.chars().count();
                        self.redraw_at(prompt, &line, cursor, &mut stdout);
                    }
                }
                term::Key::Down => {
                    if let Some(next) = self.history.next(&mut hist_idx) {
                        line = next.to_string();
                    } else {
                        hist_idx = None;
                        line.clear();
                    }
                    cursor = line.chars().count();
                    self.redraw_at(prompt, &line, cursor, &mut stdout);
                }
                _ => {}
            }
        }
    }

    /// Redraw the whole logical input line (prompt + current text), correctly
    /// even once it has wrapped past the terminal width: move up to the
    /// first row of the previous render, clear everything below (a wrapped
    /// line spans multiple physical rows, so `\x1b[K` alone -- which only
    /// clears the cursor's *current* row -- isn't enough), reprint, then
    /// reposition the cursor at its logical (row, column).
    fn redraw_at(&mut self, prompt: &str, line: &str, cursor: usize, stdout: &mut io::Stdout) {
        let display = if self.color {
            self.highlighter.highlight(line)
        } else {
            line.to_string()
        };
        let width = term::width();
        let prompt_chars = prompt.chars().count();
        let line_chars = line.chars().count();
        let (_, end_row, end_col) = layout(prompt_chars, line_chars, line_chars, width);
        let (_, target_row, target_col) = layout(prompt_chars, line_chars, cursor, width);

        if self.prev_cursor_row > 0 {
            print!("\x1b[{}A", self.prev_cursor_row);
        }
        print!("\r\x1b[J{prompt}{display}");

        // After printing, the physical cursor sits at (end_row, end_col) --
        // unless `end_col == width` (the rendered length is an exact
        // multiple of the terminal width), where the terminal defers
        // advancing to a new row until the next character is actually
        // written ("pending wrap"). Moving the cursor up while in that
        // state is unreliable across terminals, so force the wrap
        // deterministically first whenever we need to move away from it.
        let mut current_row = end_row;
        if target_row < end_row && end_col == width {
            println!();
            current_row += 1;
        }

        let up = current_row - target_row;
        if up > 0 {
            print!("\x1b[{up}A");
        }
        print!("\r");
        if target_col > 0 {
            print!("\x1b[{target_col}C");
        }

        self.prev_cursor_row = target_row;
        stdout.flush().ok();
    }

    /// Move the cursor to the last physical row of the current render
    /// before newlining on Enter/Ctrl-C/Ctrl-D -- otherwise, if the cursor
    /// is left on an earlier wrapped row, the newline's output would land
    /// on top of the remaining wrapped rows below it instead of after them.
    fn finish_line(&self, prompt: &str, line: &str) {
        let width = term::width();
        let prompt_chars = prompt.chars().count();
        let line_chars = line.chars().count();
        let (_, end_row, _) = layout(prompt_chars, line_chars, line_chars, width);
        let down = end_row.saturating_sub(self.prev_cursor_row);
        if down > 0 {
            print!("\x1b[{down}B");
        }
        println!();
    }
}

/// Compute how many physical rows the rendered `prompt`+`line` occupy at
/// terminal `width` columns, and which (row, column) the cursor -- at char
/// position `cursor` within `line` -- lands on. `rows`/`cursor_row` are
/// 0-indexed from the first row of the rendered text.
///
/// All size inputs are **character** counts of the uncolored text --
/// ANSI color escapes have zero visible width and must never be counted
/// here (the caller measures `prompt`/`line`, never the colorized display
/// string).
///
/// A cursor at the true end of the line (`cursor == line_chars`) is
/// resolved from `rows` directly rather than `pos / width`: when the
/// rendered length is an exact multiple of `width`, terminals defer
/// wrapping to a new row until the next character is actually written (the
/// "pending wrap" state), so `pos / width` would place the cursor one row
/// past where it physically still is.
fn layout(
    prompt_chars: usize,
    line_chars: usize,
    cursor: usize,
    width: usize,
) -> (usize, usize, usize) {
    let width = width.max(1);
    let total = prompt_chars + line_chars;
    let rows = if total == 0 {
        1
    } else {
        (total - 1) / width + 1
    };
    if cursor == line_chars {
        let row = rows - 1;
        let col = total - row * width;
        (rows, row, col)
    } else {
        let pos = prompt_chars + cursor;
        (rows, pos / width, pos % width)
    }
}

#[cfg(test)]
mod layout_tests {
    use super::layout;

    #[test]
    fn no_wrap_fits_on_one_row() {
        // prompt "pr" (2) + line "abc" (3), cursor after 2 chars, width 80.
        assert_eq!(layout(2, 3, 2, 80), (1, 0, 4));
    }

    #[test]
    fn exact_wrap_boundary_is_pending_wrap_at_cursor() {
        // 10 chars exactly fill a 10-wide row; cursor at the true end lands
        // in the "pending wrap" cell (row 0, col == width), not row 1 col 0.
        assert_eq!(layout(0, 10, 10, 10), (1, 0, 10));
    }

    #[test]
    fn cursor_on_first_row_of_a_two_row_line() {
        // prompt 0, line 8 chars, width 5 -> 2 rows; cursor at 3 is row 0.
        assert_eq!(layout(0, 8, 3, 5), (2, 0, 3));
    }

    #[test]
    fn cursor_on_last_row_of_a_two_row_line() {
        // Same line/width; cursor at the true end (8) is row 1, col 3.
        assert_eq!(layout(0, 8, 8, 5), (2, 1, 3));
    }

    #[test]
    fn width_one_is_degenerate_but_does_not_panic() {
        // Every character is its own row; cursor at the true end lands in
        // the pending-wrap cell of the last row.
        assert_eq!(layout(0, 3, 3, 1), (3, 2, 1));
        // An interior cursor sits at the start of its own row.
        assert_eq!(layout(0, 3, 1, 1), (3, 1, 0));
    }

    #[test]
    fn zero_width_is_clamped_to_one_rather_than_dividing_by_zero() {
        assert_eq!(layout(0, 2, 2, 0), layout(0, 2, 2, 1));
    }

    #[test]
    fn empty_line_is_one_row_at_the_prompt_column() {
        assert_eq!(layout(4, 0, 0, 80), (1, 0, 4));
    }
}

/// Previous word boundary (char index) from `cursor` in `line`, whitespace-
/// delimited: skip any whitespace immediately before the cursor, then skip
/// back through the word before that -- readline/emacs's Ctrl-W / Alt-B
/// convention. Operates on char indices, never bytes, so it's safe on
/// multibyte input.
fn prev_word_boundary(line: &str, cursor: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = cursor.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// Next word boundary (char index) from `cursor` in `line`: skip the rest
/// of the current word, then any whitespace after it -- Alt-F's
/// convention.
fn next_word_boundary(line: &str, cursor: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = cursor.min(len);
    while i < len && !chars[i].is_whitespace() {
        i += 1;
    }
    while i < len && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

#[cfg(test)]
mod word_boundary_tests {
    use super::{next_word_boundary, prev_word_boundary};

    #[test]
    fn prev_boundary_from_end_of_last_word() {
        assert_eq!(prev_word_boundary("hello world", 11), 6);
    }

    #[test]
    fn prev_boundary_skips_multiple_spaces() {
        assert_eq!(prev_word_boundary("hello   world", 13), 8);
    }

    #[test]
    fn prev_boundary_with_leading_spaces_stops_at_zero() {
        assert_eq!(prev_word_boundary("   hello", 8), 3);
        assert_eq!(prev_word_boundary("   hello", 2), 0);
    }

    #[test]
    fn prev_boundary_mid_word_stops_at_word_start() {
        assert_eq!(prev_word_boundary("hello world", 8), 6);
    }

    #[test]
    fn prev_boundary_at_start_is_zero() {
        assert_eq!(prev_word_boundary("hello", 0), 0);
    }

    #[test]
    fn prev_boundary_handles_multibyte_chars() {
        // "héllo wörld": both non-ASCII chars count as one char each.
        assert_eq!(prev_word_boundary("héllo wörld", 11), 6);
    }

    #[test]
    fn next_boundary_from_start_of_first_word() {
        assert_eq!(next_word_boundary("hello world", 0), 6);
    }

    #[test]
    fn next_boundary_skips_multiple_spaces() {
        assert_eq!(next_word_boundary("hello   world", 0), 8);
    }

    #[test]
    fn next_boundary_with_trailing_spaces_stops_at_end() {
        assert_eq!(next_word_boundary("hello   ", 0), 8);
    }

    #[test]
    fn next_boundary_mid_word_skips_to_start_of_next_word() {
        assert_eq!(next_word_boundary("hello world", 2), 6);
    }

    #[test]
    fn next_boundary_at_end_is_line_length() {
        assert_eq!(next_word_boundary("hello", 5), 5);
    }

    #[test]
    fn next_boundary_handles_multibyte_chars() {
        assert_eq!(next_word_boundary("héllo wörld", 0), 6);
    }
}

/// The longest string every one of `strings` starts with (char-wise), for
/// Tab's "partial completion" of multiple candidates. Empty for an empty
/// input or when the strings share no common prefix.
fn longest_common_prefix(strings: &[String]) -> String {
    let Some(first) = strings.first() else {
        return String::new();
    };
    let mut result: Vec<char> = first.chars().collect();
    for s in &strings[1..] {
        let s_chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < result.len() && i < s_chars.len() && result[i] == s_chars[i] {
            i += 1;
        }
        result.truncate(i);
        if result.is_empty() {
            break;
        }
    }
    result.into_iter().collect()
}

#[cfg(test)]
mod longest_common_prefix_tests {
    use super::longest_common_prefix;

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn single_string_is_itself() {
        assert_eq!(longest_common_prefix(&["select".to_string()]), "select");
    }

    #[test]
    fn shared_prefix_across_several_strings() {
        let strings = ["select".to_string(), "self".to_string()];
        assert_eq!(longest_common_prefix(&strings), "sel");
    }

    #[test]
    fn no_shared_prefix_is_empty() {
        let strings = ["select".to_string(), "from".to_string()];
        assert_eq!(longest_common_prefix(&strings), "");
    }

    #[test]
    fn one_string_being_a_prefix_of_another() {
        let strings = ["order".to_string(), "order_by".to_string()];
        assert_eq!(longest_common_prefix(&strings), "order");
    }

    #[test]
    fn handles_multibyte_chars() {
        let strings = ["héllo".to_string(), "héllx".to_string()];
        assert_eq!(longest_common_prefix(&strings), "héll");
    }
}

fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod char_byte_index_tests {
    use super::char_byte_index;

    #[test]
    fn ascii_index_matches_byte_offset() {
        assert_eq!(char_byte_index("hello", 2), 2);
    }

    #[test]
    fn index_at_or_past_end_returns_byte_len() {
        assert_eq!(char_byte_index("hi", 2), 2);
        assert_eq!(char_byte_index("hi", 100), 2);
    }

    #[test]
    fn empty_string_returns_zero() {
        assert_eq!(char_byte_index("", 0), 0);
    }

    #[test]
    fn multibyte_chars_use_char_count_not_byte_count() {
        // "héllo": 'é' is 2 bytes, so char index 2 ('l') is at byte 3.
        let s = "héllo";
        assert_eq!(char_byte_index(s, 2), 3);
        assert_eq!(char_byte_index(s, 0), 0);
    }
}

fn is_tty() -> bool {
    term::is_tty()
}

/// Terminal raw-mode handling and key decoding. Requires `unsafe` (via
/// `libc`) to call `tcgetattr`/`tcsetattr` and query `isatty` — there is no
/// safe stdlib API for raw terminal I/O, so this is the crate's escape hatch
/// from `#![forbid(unsafe_code)]`.
#[allow(unsafe_code)]
mod term {
    use std::io::{self, Read};

    #[derive(Debug)]
    pub enum Key {
        Char(char),
        /// `ESC <c>` for some non-`[` byte `c` (an Alt/Meta-modified key,
        /// e.g. Alt-B/Alt-F for word-left/word-right).
        Alt(char),
        Up,
        Down,
        Left,
        Right,
        Backspace,
        Delete,
        Home,
        End,
        Unknown,
    }

    /// RAII guard for raw terminal mode.
    pub struct RawMode {
        #[cfg(unix)]
        original: libc::termios,
    }

    impl RawMode {
        #[cfg(unix)]
        pub fn enable() -> Option<Self> {
            use std::mem::MaybeUninit;
            use std::os::unix::io::AsRawFd;

            let fd = io::stdin().as_raw_fd();

            // SAFETY: Getting terminal attributes on a valid fd is safe.
            let mut original = MaybeUninit::uninit();
            if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
                return None;
            }
            let original = unsafe { original.assume_init() };

            let mut raw = original;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;

            // SAFETY: Setting terminal attributes on a valid fd is safe.
            if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
                return None;
            }

            Some(RawMode { original })
        }

        #[cfg(not(unix))]
        pub fn enable() -> Option<Self> {
            None
        }
    }

    #[cfg(unix)]
    impl Drop for RawMode {
        fn drop(&mut self) {
            use std::os::unix::io::AsRawFd;
            let fd = io::stdin().as_raw_fd();
            // SAFETY: Restoring previously-read terminal attributes is safe.
            unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &self.original) };
        }
    }

    fn read_exact_eintr(buf: &mut [u8]) -> io::Result<()> {
        loop {
            match io::stdin().read_exact(buf) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn read_eintr(buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match io::stdin().read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn is_tty() -> bool {
        // SAFETY: isatty is a simple query on a valid, always-open fd (stdin).
        unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
    }

    /// Terminal width in columns, re-queried on every redraw so a resize
    /// takes effect on the next keystroke (no `SIGWINCH` handling). Falls
    /// back to 80 when the query fails (e.g. stdout isn't a tty).
    #[cfg(unix)]
    pub fn width() -> usize {
        // SAFETY: ioctl on a valid, always-open fd (stdout) with a
        // correctly-sized, zero-initialized out-parameter is safe; a
        // non-tty fd or any other failure just yields ws_col == 0, handled
        // below by falling back to the default.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        if ok && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            80
        }
    }

    #[cfg(not(unix))]
    pub fn width() -> usize {
        80
    }

    /// What immediately follows an `ESC` byte decides between a CSI escape
    /// sequence (`ESC [ ...`, arrows/Home/End/Delete) and an Alt-modified
    /// key (`ESC <c>` for any other `c`) -- pulled out of [`read_key`] as a
    /// pure function so this decision is unit-testable without a tty.
    fn decode_escape(first: u8) -> Option<char> {
        if first == b'[' {
            None
        } else {
            Some(first as char)
        }
    }

    /// Read a single key from stdin (raw mode).
    pub fn read_key() -> io::Result<Key> {
        let mut buf = [0u8; 1];
        read_exact_eintr(&mut buf)?;

        match buf[0] {
            0x1b => {
                let mut first = [0u8; 1];
                if read_eintr(&mut first)? != 1 {
                    return Ok(Key::Unknown);
                }
                let Some(alt) = decode_escape(first[0]) else {
                    let mut seq = [0u8; 1];
                    if read_eintr(&mut seq)? == 1 {
                        match seq[0] {
                            b'A' => return Ok(Key::Up),
                            b'B' => return Ok(Key::Down),
                            b'C' => return Ok(Key::Right),
                            b'D' => return Ok(Key::Left),
                            b'H' => return Ok(Key::Home),
                            b'F' => return Ok(Key::End),
                            b'3' => {
                                let mut tilde = [0u8; 1];
                                if read_exact_eintr(&mut tilde).is_ok() && tilde[0] == b'~' {
                                    return Ok(Key::Delete);
                                }
                            }
                            _ => {}
                        }
                    }
                    return Ok(Key::Unknown);
                };
                Ok(Key::Alt(alt))
            }
            0x7f => Ok(Key::Backspace),
            b => Ok(Key::Char(b as char)),
        }
    }

    // ANSI color codes, shared with `highlight`.
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD_BLUE: &str = "\x1b[1;34m";
    pub const GREEN: &str = "\x1b[32m";
    pub const CYAN: &str = "\x1b[36m";
    pub const YELLOW: &str = "\x1b[33m";

    #[cfg(test)]
    mod decode_escape_tests {
        use super::decode_escape;

        #[test]
        fn esc_followed_by_bracket_is_a_csi_sequence() {
            assert_eq!(decode_escape(b'['), None);
        }

        #[test]
        fn esc_followed_by_non_bracket_decodes_as_alt() {
            assert_eq!(decode_escape(b'b'), Some('b'));
            assert_eq!(decode_escape(b'f'), Some('f'));
            assert_eq!(decode_escape(b'x'), Some('x'));
        }
    }
}

/// SQL syntax highlighting, generic across any SQL-speaking engine.
mod highlight {
    use super::term;

    const KEYWORDS: &[&str] = &[
        "SELECT",
        "FROM",
        "WHERE",
        "GROUP",
        "BY",
        "ORDER",
        "LIMIT",
        "AND",
        "OR",
        "NOT",
        "IN",
        "AS",
        "JOIN",
        "INNER",
        "LEFT",
        "RIGHT",
        "OUTER",
        "ON",
        "NULL",
        "TRUE",
        "FALSE",
        "ASC",
        "DESC",
        "COUNT",
        "SUM",
        "AVG",
        "MIN",
        "MAX",
        "OVER",
        "PARTITION",
        "ROW_NUMBER",
        "RANK",
        "DENSE_RANK",
        "LAG",
        "LEAD",
        "FIRST_VALUE",
        "LAST_VALUE",
    ];

    /// Colorize SQL input for display.
    pub fn highlight(line: &str) -> String {
        if line.trim_start().starts_with('.') {
            return format!("{}{line}{}", term::YELLOW, term::RESET);
        }

        let mut result = String::with_capacity(line.len() + 64);
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let c = chars[i];

            if c == '\'' {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                result.push_str(term::GREEN);
                result.push_str(&s);
                result.push_str(term::RESET);
                continue;
            }

            if c.is_ascii_digit() {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                result.push_str(term::CYAN);
                result.push_str(&s);
                result.push_str(term::RESET);
                continue;
            }

            if c.is_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let upper = word.to_ascii_uppercase();
                if KEYWORDS.contains(&upper.as_str()) {
                    result.push_str(term::BOLD_BLUE);
                    result.push_str(&word);
                    result.push_str(term::RESET);
                } else {
                    result.push_str(&word);
                }
                continue;
            }

            result.push(c);
            i += 1;
        }

        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn dot_commands_highlight_whole_line() {
            assert!(highlight(".help").starts_with(term::YELLOW));
        }

        #[test]
        fn keyword_gets_colored() {
            let out = highlight("SELECT 1");
            assert!(out.contains(term::BOLD_BLUE));
        }

        #[test]
        fn plain_identifier_is_uncolored_passthrough() {
            assert_eq!(highlight("foo"), "foo");
        }
    }
}
