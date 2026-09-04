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
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let n: usize = if j > start {
                    std::str::from_utf8(&bytes[start..j])
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
        let (pty, slave) = Pty::open(ROWS as u16, COLS as u16);
        let bin = env!("CARGO_BIN_EXE_redraw-harness");
        let child = unsafe {
            Command::new(bin)
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
