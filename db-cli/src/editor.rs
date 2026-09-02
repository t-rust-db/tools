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
                    println!();
                    return Ok(line);
                }
                term::Key::Char('\x03') => {
                    // Ctrl-C
                    println!();
                    return Err(ReadlineError::Interrupted);
                }
                term::Key::Char('\x04') => {
                    // Ctrl-D
                    if line.is_empty() {
                        println!();
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

    fn redraw_at(&self, prompt: &str, line: &str, cursor: usize, stdout: &mut io::Stdout) {
        let display = if self.color {
            highlight::highlight(line)
        } else {
            line.to_string()
        };
        // Clear line, reprint, then move the cursor back to its logical position.
        let trailing = line.chars().count() - cursor;
        print!("\r\x1b[K{prompt}{display}");
        if trailing > 0 {
            print!("\x1b[{trailing}D");
        }
        stdout.flush().ok();
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
