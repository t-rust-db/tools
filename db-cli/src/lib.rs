//! Generic REPL/readline infrastructure shared across t-rust-db engine CLIs
//! (column-rs, sqlite-rs, loglume, …). Contains no engine-specific types —
//! an engine plugs in its own query execution and formatting via
//! [`ReplHandler`].
// `forbid` can't be lifted anywhere, but `editor::term` needs `libc` calls
// for raw-mode terminal I/O (no safe stdlib API exists for that), so this
// crate uses `deny` at the root and a narrow `allow` on that one module.
#![deny(unsafe_code)]

pub mod editor;
pub mod history;
pub mod output;
pub mod repl;

pub use editor::{Readline, ReadlineError};
pub use history::{history_path, History};
pub use output::{render, render_csv, render_json, render_table, OutputMode};
pub use repl::{run_repl, Repl, ReplHandler, ReplOptions, Step};
