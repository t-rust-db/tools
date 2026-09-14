//! Visual theme: a catppuccin mocha palette + rounded borders everywhere,
//! replacing the ad hoc `Color::White`/`Color::Red` calls M1 shipped with
//! (db-studio#12). Every color below is individually overridable via
//! `config.toml`'s `[theme]` table (db-studio#61, parity with
//! `t-rust-db/loglume#35`) -- [`init`] loads the resolved overrides once
//! at startup into a process-global, so none of this module's many call
//! sites elsewhere in the app need to change.

use std::sync::OnceLock;

use catppuccin::PALETTE;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

use crate::config::ThemeConfig;

static OVERRIDES: OnceLock<ThemeConfig> = OnceLock::new();

/// Installs the resolved `[theme]` overrides for the rest of this
/// module's functions to read -- call once, before the first frame
/// renders. Not calling this at all (e.g. in tests) is equivalent to
/// an empty `ThemeConfig`: every color falls back to its catppuccin
/// mocha default, unchanged from before db-studio#61.
pub fn init(overrides: ThemeConfig) {
    // A second `init` call (there shouldn't be one -- `main` calls this
    // exactly once) would silently lose its argument; not worth a panic
    // over a theme color, so this stays best-effort like `history`'s
    // own persistence.
    let _ = OVERRIDES.set(overrides);
}

fn overrides() -> &'static ThemeConfig {
    OVERRIDES.get_or_init(ThemeConfig::default)
}

/// Parses a `#rrggbb` or `rrggbb` hex string into a `Color::Rgb`.
/// `None` for anything else (wrong length, non-hex digits) -- an
/// invalid override falls back to the default rather than panicking
/// or rendering an arbitrary wrong color.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Resolves one theme color: the override if set and valid hex, else
/// `default`.
fn resolve(hex: &Option<String>, default: Color) -> Color {
    hex.as_deref().and_then(parse_hex).unwrap_or(default)
}

fn mocha() -> &'static catppuccin::FlavorColors {
    &PALETTE.mocha.colors
}

pub fn text() -> Color {
    resolve(&overrides().text, mocha().text.into())
}

pub fn subtext() -> Color {
    resolve(&overrides().subtext, mocha().subtext0.into())
}

pub fn base() -> Color {
    resolve(&overrides().base, mocha().base.into())
}

pub fn accent() -> Color {
    resolve(&overrides().accent, mocha().mauve.into())
}

pub fn error() -> Color {
    resolve(&overrides().error, mocha().red.into())
}

pub fn keyword() -> Color {
    resolve(&overrides().keyword, mocha().mauve.into())
}

pub fn string_literal() -> Color {
    resolve(&overrides().string_literal, mocha().green.into())
}

pub fn number_literal() -> Color {
    resolve(&overrides().number_literal, mocha().peach.into())
}

pub fn identifier() -> Color {
    resolve(&overrides().identifier, mocha().blue.into())
}

pub fn header() -> Color {
    resolve(&overrides().header, mocha().yellow.into())
}

pub fn selection_bg() -> Color {
    resolve(&overrides().selection_bg, mocha().surface1.into())
}

/// A pane's outer block, rounded, titled, and bordered per whether it
/// currently has keyboard focus (Longbridge Terminal's convention --
/// nothing distinguished this in M1). Generic over the title so a
/// caller with a computed title (db-studio#42's "N column(s) hidden"
/// grid title) doesn't need a separate owned-string variant.
pub fn pane_block(title: impl Into<ratatui::text::Line<'static>>, focused: bool) -> Block<'static> {
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

/// A borderless pane background/foreground, no title chrome -- for the
/// query and error panes, which are dense enough (and visited often
/// enough while typing) that a border felt like pure noise rather than
/// a useful frame.
pub fn pane_block_borderless() -> Block<'static> {
    Block::default()
        .borders(Borders::NONE)
        .style(Style::default().bg(base()).fg(text()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_with_and_without_hash() {
        assert_eq!(parse_hex("#ff00ff"), Some(Color::Rgb(0xff, 0x00, 0xff)));
        assert_eq!(parse_hex("ff00ff"), Some(Color::Rgb(0xff, 0x00, 0xff)));
    }

    #[test]
    fn parse_hex_rejects_wrong_length_or_non_hex() {
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#gggggg"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn resolve_prefers_a_valid_override_over_the_default() {
        let default = Color::Rgb(1, 2, 3);
        assert_eq!(
            resolve(&Some("#010203".to_string()), default),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            resolve(&Some("#00ff00".to_string()), default),
            Color::Rgb(0, 0xff, 0)
        );
    }

    #[test]
    fn resolve_falls_back_to_the_default_when_unset_or_invalid() {
        let default = Color::Rgb(9, 9, 9);
        assert_eq!(resolve(&None, default), default);
        assert_eq!(resolve(&Some("not-a-color".to_string()), default), default);
    }

    /// Without a call to `init` (the case for every other test in this
    /// crate, and any test built around a real `App`), every color
    /// falls back to its catppuccin mocha default -- db-studio#61 adds
    /// no behavior change for code that never touches `config.rs`.
    #[test]
    fn uninitialized_overrides_default_to_catppuccin_mocha() {
        assert_eq!(text(), Color::from(mocha().text));
        assert_eq!(accent(), Color::from(mocha().mauve));
    }
}
