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

pub struct Readline {
    history: History,
    tty: bool,
    color: bool,
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
            prev_cursor_row: 0,
        }
    }

    pub fn set_color(&mut self, enabled: bool) {
        self.color = enabled;
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
            highlight::highlight(line)
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

    /// Read a single key from stdin (raw mode).
    pub fn read_key() -> io::Result<Key> {
        let mut buf = [0u8; 1];
        read_exact_eintr(&mut buf)?;

        match buf[0] {
            0x1b => {
                let mut seq = [0u8; 2];
                if read_eintr(&mut seq)? == 2 && seq[0] == b'[' {
                    match seq[1] {
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
                Ok(Key::Unknown)
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
