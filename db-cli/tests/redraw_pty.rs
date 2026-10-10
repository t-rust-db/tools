//! End-to-end regression test for issue #1 (readline redraw corrupts the
//! display once input wraps past the terminal width): drives the
//! `redraw-harness` binary over a real pty, narrowed to a fixed column
//! width, and replays the exact sequence from the bug report -- type past
//! the width, edit across the wrap boundary, press Enter -- then checks
//! the resulting virtual screen for the corruption the issue described
//! (a reprinted prompt embedded mid-row) and for the expected final
//! cursor/row layout.
//!
//! Complements `editor::layout_tests` (the pure row/column math, unit
//! tested in isolation): this test instead verifies the ANSI byte stream
//! `redraw_at`/`finish_line` actually emit renders correctly end to end.
// A real pty needs libc calls; test-only, and the only unsafe outside
// `editor::term`.
#![allow(unsafe_code, reason = "pty setup via libc in a test harness")]
#![allow(
    clippy::let_underscore_must_use,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::ffi::CStr;
use std::os::unix::io::{FromRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const COLS: usize = 20;
const ROWS: usize = 10;

/// A pty pair: `master` stays with the test process, `slave` is handed to
/// the child as its controlling stdin/stdout so `isatty`/`tcgetattr` in
/// `db_cli::editor::term` see a real terminal device.
struct Pty {
    master: RawFd,
}

impl Pty {
    fn open(rows: u16, cols: u16) -> (Self, RawFd) {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(libc::grantpt(master), 0, "grantpt failed");
            assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");

            let name_ptr = libc::ptsname(master);
            assert!(!name_ptr.is_null(), "ptsname failed");
            let slave_path = CStr::from_ptr(name_ptr).to_owned();

            let slave = libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "open(slave) failed");

            let ws = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                libc::ioctl(slave, libc::TIOCSWINSZ, &ws),
                0,
                "TIOCSWINSZ failed"
            );

            (Pty { master }, slave)
        }
    }

    fn write(&self, bytes: &[u8]) {
        unsafe {
            let n = libc::write(self.master, bytes.as_ptr() as *const _, bytes.len());
            assert_eq!(n, bytes.len() as isize, "short write to pty master");
        }
    }

    /// Drain whatever the child has written so far: poll for up to
    /// `timeout` total, stopping early once a short quiet gap follows some
    /// output (the child writes asynchronously in response to each input
    /// byte, so there's no fixed message boundary to wait for).
    fn read_available(&self, timeout: Duration) -> Vec<u8> {
        unsafe {
            let flags = libc::fcntl(self.master, libc::F_GETFL);
            libc::fcntl(self.master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let mut quiet_polls_since_data = 0;
        loop {
            let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                quiet_polls_since_data = 0;
            } else if !out.is_empty() {
                quiet_polls_since_data += 1;
                if quiet_polls_since_data >= 2 {
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        out
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.master);
        }
    }
}

/// A tiny terminal emulator covering exactly the escape vocabulary
/// `redraw_at`/`finish_line` emit (`\r`, `\n`, cursor up/forward, clear to
/// end of screen) plus plain-character auto-wrap with the same
/// "pending wrap" deferral real terminals use -- enough to independently
/// verify the byte stream renders the way a real terminal would, without
/// pulling in a full VT100 emulator crate.
struct Screen {
    rows: Vec<Vec<char>>,
    cur_row: usize,
    cur_col: usize,
    pending_wrap: bool,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        Screen {
            rows: (0..rows).map(|_| vec![' '; cols]).collect(),
            cur_row: 0,
            cur_col: 0,
            pending_wrap: false,
        }
    }

    fn cols(&self) -> usize {
        self.rows[0].len()
    }

    fn feed(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                let mut j = i + 2;
                let start = j;
                // CSI parameter bytes are digits *and* `;` (multi-param
                // SGR color codes like `\x1b[1;34m` need the `;` skipped
                // too, or the scan stops early and "34m" leaks out as
                // literal text) -- only the leading digit run before any
                // `;` is used as `n` (fine for the single-param sequences
                // this editor actually emits for cursor movement/clear).
                while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                    j += 1;
                }
                let digit_end = bytes[start..j]
                    .iter()
                    .position(|&b| b == b';')
                    .map_or(j, |p| start + p);
                let had_digits = digit_end > start;
                let n: usize = if had_digits {
                    std::str::from_utf8(&bytes[start..digit_end])
                        .unwrap()
                        .parse()
                        .unwrap()
                } else {
                    1
                };
                if j < bytes.len() {
                    match bytes[j] {
                        b'A' => self.cur_row = self.cur_row.saturating_sub(n),
                        b'B' => self.cur_row = (self.cur_row + n).min(self.rows.len() - 1),
                        b'C' => {
                            self.cur_col = (self.cur_col + n).min(self.cols() - 1);
                            self.pending_wrap = false;
                        }
                        b'D' => {
                            self.cur_col = self.cur_col.saturating_sub(n);
                            self.pending_wrap = false;
                        }
                        // SGR color codes (`\x1b[0m`, `\x1b[1;34m`, ...) --
                        // no visible effect on this plain-text screen model.
                        b'm' => {}
                        b'J' if had_digits && n == 2 => {
                            // Whole-screen clear (Ctrl-L's `\x1b[2J`); cursor unmoved.
                            let cols = self.cols();
                            for row in self.rows.iter_mut() {
                                *row = vec![' '; cols];
                            }
                        }
                        b'J' => {
                            // Clear from cursor to end of screen.
                            let cols = self.cols();
                            for c in self.cur_col..cols {
                                self.rows[self.cur_row][c] = ' ';
                            }
                            for r in (self.cur_row + 1)..self.rows.len() {
                                self.rows[r] = vec![' '; cols];
                            }
                        }
                        b'H' => {
                            // Cursor home (Ctrl-L's `\x1b[H`, no params).
                            self.cur_row = 0;
                            self.cur_col = 0;
                            self.pending_wrap = false;
                        }
                        _ => {}
                    }
                    i = j + 1;
                    continue;
                }
            }
            match b {
                b'\r' => {
                    self.cur_col = 0;
                    self.pending_wrap = false;
                }
                b'\n' => {
                    self.cur_row = (self.cur_row + 1).min(self.rows.len() - 1);
                    self.cur_col = 0;
                    self.pending_wrap = false;
                }
                0x20..=0x7e => {
                    if self.pending_wrap {
                        self.cur_row = (self.cur_row + 1).min(self.rows.len() - 1);
                        self.cur_col = 0;
                        self.pending_wrap = false;
                    }
                    self.rows[self.cur_row][self.cur_col] = b as char;
                    self.cur_col += 1;
                    if self.cur_col == self.cols() {
                        self.pending_wrap = true;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn row_text(&self, r: usize) -> String {
        self.rows[r]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}

struct Harness {
    pty: Pty,
    child: Child,
    screen: Screen,
}

impl Harness {
    fn start() -> Self {
        Self::start_with_env(&[])
    }

    fn start_with_env(env: &[(&str, &str)]) -> Self {
        // Serialize pty allocation + child spawn across the parallel test
        // threads: a sibling test forking while this one's dup'd slave fd
        // is still open leaks that fd into the wrong child, and `grantpt`
        // has been seen failing under concurrent allocation (flaky ~1 in
        // 6 runs). The lock is released once the child owns its fds.
        static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _spawn = SPAWN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (pty, slave) = Pty::open(ROWS as u16, COLS as u16);
        let bin = env!("CARGO_BIN_EXE_redraw-harness");
        let child = unsafe {
            Command::new(bin)
                .envs(env.iter().copied())
                .stdin(Stdio::from_raw_fd(libc::dup(slave)))
                .stdout(Stdio::from_raw_fd(libc::dup(slave)))
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn redraw-harness")
        };
        unsafe {
            libc::close(slave);
        }
        let mut h = Harness {
            pty,
            child,
            screen: Screen::new(ROWS, COLS),
        };
        h.drain(); // initial prompt
        h
    }

    fn send(&mut self, bytes: &[u8]) {
        self.pty.write(bytes);
        self.drain();
    }

    fn drain(&mut self) {
        let out = self.pty.read_available(Duration::from_millis(400));
        self.screen.feed(&out);
    }

    /// Submit whatever line is in progress (so `quit` starts a fresh one
    /// rather than being inserted into leftover text) and exit the child.
    fn finish(mut self) {
        self.send(b"\n");
        self.send(b"quit\n");
        let _ = self.child.wait();
    }
}

#[test]
fn wrapped_line_redraws_without_duplicating_the_prompt() {
    let mut h = Harness::start();
    assert_eq!(h.screen.row_text(0), "column>");

    // Prompt is 8 chars ("column> "); 12 more fills row 0 exactly (20 cols),
    // then the rest wraps onto row 1 -- the exact scenario from the bug
    // report, where every subsequent keystroke used to reprint the prompt
    // at the wrap point.
    h.send(b"abcdefghijklmnopqrstuvwxyz");
    assert_eq!(h.screen.row_text(0), "column> abcdefghijkl");
    assert_eq!(h.screen.row_text(1), "mnopqrstuvwxyz");
    // The bug's signature: the prompt reprinted mid-row.
    assert!(
        !h.screen.row_text(1).contains("column>"),
        "prompt leaked onto the wrapped row: {:?}",
        h.screen.row_text(1)
    );
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (1, 14));

    // Edit across the wrap boundary: Left x3, Backspace x2, insert 'Z'.
    h.send(b"\x1b[D\x1b[D\x1b[D");
    h.send(b"\x7f\x7f");
    h.send(b"Z");
    assert_eq!(h.screen.row_text(0), "column> abcdefghijkl");
    assert_eq!(h.screen.row_text(1), "mnopqrstuZxyz");
    assert!(
        !h.screen.row_text(1).contains("column>"),
        "prompt leaked onto the wrapped row after edits: {:?}",
        h.screen.row_text(1)
    );

    // Enter must move past every wrapped row, not print over one of them.
    h.send(b"\n");
    assert_eq!(h.screen.row_text(2), "column>");
    assert_eq!(h.screen.row_text(0), "column> abcdefghijkl");
    assert_eq!(h.screen.row_text(1), "mnopqrstuZxyz");

    h.finish();
}

#[test]
fn three_row_wrap_supports_home_end_and_left_navigation() {
    let mut h = Harness::start();

    // prompt(8) + 39 chars = 47 -> ceil(47/20) = 3 rows (20, 20, 7).
    h.send(b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ012");
    assert_eq!(h.screen.row_text(0), "column> 0123456789AB");
    assert_eq!(h.screen.row_text(1), "CDEFGHIJKLMNOPQRSTUV");
    assert_eq!(h.screen.row_text(2), "WXYZ012");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (2, 7));

    h.send(b"\x1b[H"); // Home
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 8));

    h.send(b"\x1b[F"); // End
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (2, 7));

    h.send(&b"\x1b[D".repeat(30)); // Left x30
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 17));

    // No corruption anywhere on screen after all that navigation.
    assert_eq!(h.screen.row_text(0), "column> 0123456789AB");
    assert_eq!(h.screen.row_text(1), "CDEFGHIJKLMNOPQRSTUV");
    assert_eq!(h.screen.row_text(2), "WXYZ012");

    h.finish();
}

/// #2: Ctrl-A/E/B/F are emacs/readline aliases for Home/End/Left/Right.
#[test]
fn ctrl_a_e_b_f_move_cursor_like_home_end_left_right() {
    let mut h = Harness::start();

    h.send(b"hello world"); // prompt(8) + 11 = 19, fits on one row.
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 19));

    h.send(b"\x01"); // Ctrl-A
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 8));

    h.send(b"\x05"); // Ctrl-E
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 19));

    h.send(b"\x02"); // Ctrl-B
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 18));

    h.send(b"\x06"); // Ctrl-F
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 19));

    h.finish();
}

/// #2: Ctrl-K kills to end of line, Ctrl-U kills to start, Ctrl-W kills the
/// previous word.
#[test]
fn ctrl_k_u_w_kill_bindings() {
    let mut h = Harness::start();

    h.send(b"hello world");
    h.send(b"\x01"); // Ctrl-A -> cursor 0
    h.send(b"\x06\x06\x06\x06\x06\x06"); // Ctrl-F x6 -> cursor after "hello "
    h.send(b"\x0b"); // Ctrl-K: kill to end of line
    assert_eq!(h.screen.row_text(0), "column> hello");

    h.send(b"\x15"); // Ctrl-U: kill to start of line
    assert_eq!(h.screen.row_text(0), "column>");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 8));

    h.send(b"foo   bar"); // multiple spaces between words
    h.send(b"\x17"); // Ctrl-W: kill the previous word ("bar")
    assert_eq!(h.screen.row_text(0), "column> foo");

    h.finish();
}

/// #2: Alt-B/Alt-F move by word (whitespace-delimited), sharing the same
/// boundary logic as Ctrl-W.
#[test]
fn alt_b_and_alt_f_move_by_word() {
    let mut h = Harness::start();

    h.send(b"foo bar baz"); // prompt(8) + 11 = 19; cursor at true end.
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 19));

    h.send(b"\x1bb"); // Alt-B -> start of "baz"
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 16));

    h.send(b"\x1bb"); // Alt-B -> start of "bar"
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 12));

    h.send(b"\x1bf"); // Alt-F -> start of "baz" (word end + trailing space)
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 16));

    h.finish();
}

/// #2: Ctrl-L clears the screen and redraws the current line at the top.
#[test]
fn ctrl_l_clears_screen_and_redraws_at_top() {
    let mut h = Harness::start();

    h.send(b"hello");
    h.send(b"\x0c"); // Ctrl-L
    assert_eq!(h.screen.row_text(0), "column> hello");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 13));
    // Nothing left over from wherever the cursor was before the clear.
    assert_eq!(h.screen.row_text(1), "");

    h.finish();
}

/// #2: Ctrl-P/Ctrl-N are emacs/readline aliases for history Up/Down.
#[test]
fn ctrl_p_n_navigate_history_like_up_down() {
    let mut h = Harness::start();

    h.send(b"first line");
    h.send(b"\n");
    h.send(b"second");
    h.send(b"\n");

    h.send(b"\x10"); // Ctrl-P -> most recent ("second")
    assert_eq!(h.screen.row_text(2), "column> second");

    h.send(b"\x10"); // Ctrl-P -> older ("first line")
    assert_eq!(h.screen.row_text(2), "column> first line");

    h.send(b"\x0e"); // Ctrl-N -> back to "second"
    assert_eq!(h.screen.row_text(2), "column> second");

    h.send(b"\x0e"); // Ctrl-N -> past the newest entry, back to blank
    assert_eq!(h.screen.row_text(2), "column>");

    h.finish();
}

/// #2: word motion (Alt-B here) must still work correctly once the line has
/// wrapped past the terminal width -- the redraw-wrap fix (#1) is exactly
/// what a broken redraw would show up as at a wrap point.
#[test]
fn alt_b_word_motion_works_across_a_wrapped_line() {
    let mut h = Harness::start();

    // prompt(8) + "aaaa bbbb cccc dddd" (19 chars) = 27 -> wraps onto row 1.
    h.send(b"aaaa bbbb cccc dddd");
    assert_eq!(h.screen.row_text(0), "column> aaaa bbbb cc");
    assert_eq!(h.screen.row_text(1), "cc dddd");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (1, 7));

    h.send(b"\x1bb"); // Alt-B -> start of "dddd"
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (1, 3));
    assert!(
        !h.screen.row_text(1).contains("column>"),
        "prompt leaked onto the wrapped row during word motion: {:?}",
        h.screen.row_text(1)
    );

    h.finish();
}

/// #6: Tab does nothing when no `Completer` is installed (default
/// behavior unchanged).
#[test]
fn tab_does_nothing_without_a_completer() {
    let mut h = Harness::start();

    h.send(b"SEL");
    h.send(b"\t");
    assert_eq!(h.screen.row_text(0), "column> SEL");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 11));

    h.finish();
}

/// #6: Tab with exactly one matching candidate completes the word
/// outright.
#[test]
fn tab_with_a_single_candidate_completes_the_word() {
    let mut h = Harness::start_with_env(&[("DB_CLI_TEST_COMPLETER", "1")]);

    h.send(b"WHE");
    h.send(b"\t"); // Only "WHERE" starts with "WHE" among SELECT/SELF/FROM/WHERE.
    assert_eq!(h.screen.row_text(0), "column> WHERE");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 13));

    h.finish();
}

/// #6: Tab with multiple candidates inserts their longest common prefix
/// (classic readline "partial completion") instead of picking one.
#[test]
fn tab_with_multiple_candidates_completes_the_longest_common_prefix() {
    let mut h = Harness::start_with_env(&[("DB_CLI_TEST_COMPLETER", "1")]);

    h.send(b"SE");
    h.send(b"\t"); // "SELECT" and "SELF" both match "SE"; common prefix is "SEL".
    assert_eq!(h.screen.row_text(0), "column> SEL");

    // Tab again: still ambiguous (both "SELECT"/"SELF" still start with
    // "SEL"), and the common prefix is already exactly what's typed, so
    // nothing changes.
    h.send(b"\t");
    assert_eq!(h.screen.row_text(0), "column> SEL");

    h.finish();
}

/// #6: Tab does nothing when the prefix at the cursor matches no
/// candidate.
#[test]
fn tab_with_no_matching_candidates_does_nothing() {
    let mut h = Harness::start_with_env(&[("DB_CLI_TEST_COMPLETER", "1")]);

    h.send(b"XYZ");
    h.send(b"\t");
    assert_eq!(h.screen.row_text(0), "column> XYZ");

    h.finish();
}

/// #6: a plugged-in `Highlighter` replaces the default keyword-based one.
#[test]
fn custom_highlighter_replaces_the_default() {
    let mut h = Harness::start_with_env(&[("DB_CLI_TEST_HIGHLIGHTER", "1")]);

    h.send(b"hello");
    // TestHighlighter upper-cases the line.
    assert_eq!(h.screen.row_text(0), "column> HELLO");
    assert_eq!((h.screen.cur_row, h.screen.cur_col), (0, 13));

    h.finish();
}
