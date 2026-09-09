//! `db-studio`: a TUI database studio for the t-rust-db family. See
//! `.openspec/plan.md` for scope and phasing.

mod app;
mod terminal;

use std::io;

fn main() -> io::Result<()> {
    let mut term = terminal::init()?;
    let result = app::App::new().run(&mut term);
    terminal::restore()?;
    result
}
