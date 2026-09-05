//! Generic REPL/readline infrastructure shared across t-rust-db engine CLIs
//! (column-rs, sqlite-rs, loglume, …). Contains no engine-specific types —
//! an engine plugs in its own query execution and formatting via
//! [`ReplHandler`].
// `forbid` can't be lifted anywhere, but `editor::term` needs `libc` calls
// for raw-mode terminal I/O (no safe stdlib API exists for that), so this
// crate uses `deny` at the root and a narrow `allow` on that one module.
#![deny(unsafe_code)]
#![warn(missing_docs)]
// Unchecked cursor arithmetic and byte-index slicing in the line editor
// predate the lint bar (#5); burned down site by site in #8, then this
// allowance goes.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "arithmetic_side_effects / indexing_slicing burn-down tracked in #8"
)]

pub mod editor;
pub mod history;
pub mod output;
pub mod repl;

pub use editor::{Completer, Highlighter, Readline, ReadlineError};
pub use history::{history_path, History};
pub use output::{
    render, render_column, render_csv, render_json, render_line, render_list, render_table,
    OutputMode,
};
pub use repl::{
    run_repl, run_repl_with, run_repl_with_editor, Repl, ReplHandler, ReplOptions, Step,
};
