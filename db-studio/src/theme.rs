//! Visual theme: a catppuccin mocha palette + rounded borders everywhere,
//! replacing the ad hoc `Color::White`/`Color::Red` calls M1 shipped with
//! (db-studio#12).

use catppuccin::PALETTE;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

fn mocha() -> &'static catppuccin::FlavorColors {
    &PALETTE.mocha.colors
}

pub fn text() -> Color {
    mocha().text.into()
}

pub fn subtext() -> Color {
    mocha().subtext0.into()
}

pub fn base() -> Color {
    mocha().base.into()
}

pub fn accent() -> Color {
    mocha().mauve.into()
}

pub fn error() -> Color {
    mocha().red.into()
}

pub fn keyword() -> Color {
    mocha().mauve.into()
}

pub fn string_literal() -> Color {
    mocha().green.into()
}

pub fn number_literal() -> Color {
    mocha().peach.into()
}

pub fn identifier() -> Color {
    mocha().blue.into()
}

pub fn selection_bg() -> Color {
    mocha().surface1.into()
}

/// A pane's outer block, rounded, titled, and bordered per whether it
/// currently has keyboard focus (Longbridge Terminal's convention --
/// nothing distinguished this in M1).
pub fn pane_block(title: &'static str, focused: bool) -> Block<'static> {
    let border_color = if focused { accent() } else { subtext() };
    let mut border_style = Style::default().fg(border_color);
    if focused {
        border_style = border_style.add_modifier(Modifier::BOLD);
    }
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(base()).fg(text()))
}
