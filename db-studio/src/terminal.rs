//! Terminal setup/teardown -- raw mode + the alternate screen, restored on
//! both a clean quit and a panic (db-studio#1).

use std::io::{self, Stdout};

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Enters raw mode + the alternate screen and installs a panic hook that
/// restores the terminal first -- a panic must never leave the user's shell
/// in raw mode with no visible cursor.
pub fn init() -> io::Result<Tui> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;

    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        default_panic(info);
    }));

    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// Leaves the alternate screen and disables raw mode. Safe to call more
/// than once (e.g. once from the panic hook and once from normal
/// `main` cleanup) -- both calls' errors are swallowed by the panic-hook
/// caller, but `main`'s own call surfaces a real error.
pub fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
