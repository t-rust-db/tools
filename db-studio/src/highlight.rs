//! SQL syntax highlighting for the query pane (db-studio#9), classified
//! by `db-core`'s real row tokenizer -- the same tokens the engine will
//! actually compile, not a hand-maintained keyword list (contrast
//! `db-cli/src/editor.rs`'s `highlight` module, which is exactly that,
//! for a line-editor's ANSI-escape output rather than a TUI widget).

use db_core::parser::row::tokenizer::{Token, TokenKind, Tokenizer};
use ratatui::style::Style;

use crate::theme;

/// One highlighted range: `((start_row, start_col), (end_row, end_col))`
/// in [`tui_textarea_2::TextArea::custom_highlight`]'s coordinates
/// (0-based, end-exclusive on the column), plus the style to apply.
pub type Highlight = ((usize, usize), (usize, usize), Style);

/// Tokenizes `text` and returns one highlight per token worth coloring
/// (punctuation and EOF are left unstyled). `db-core`'s [`Token::span`]
/// is already 1-based line/column, so this is a straight translation,
/// not a re-derivation of source positions.
pub fn highlights(text: &str) -> Vec<Highlight> {
    Tokenizer::tokenize(text)
        .iter()
        .filter_map(token_highlight)
        .collect()
}

fn token_highlight(token: &Token) -> Option<Highlight> {
    let style = style_for(&token.kind)?;
    if token.span.is_unknown() {
        return None;
    }
    let row = (token.span.line.max(1) - 1) as usize;
    let col = (token.span.column.max(1) - 1) as usize;
    let end_col = col + token.span.len as usize;
    Some(((row, col), (row, end_col), style))
}

fn style_for(kind: &TokenKind) -> Option<Style> {
    let color = match kind {
        TokenKind::Keyword(_) | TokenKind::Null | TokenKind::True | TokenKind::False => {
            theme::keyword()
        }
        TokenKind::String(_) | TokenKind::Blob(_) => theme::string_literal(),
        TokenKind::Integer(_) | TokenKind::Float(_) => theme::number_literal(),
        TokenKind::Identifier(_) => theme::identifier(),
        TokenKind::Error(_) => theme::error(),
        _ => return None,
    };
    Some(Style::default().fg(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds_at(text: &str) -> Vec<(usize, usize, usize, usize)> {
        highlights(text)
            .into_iter()
            .map(|((sr, sc), (er, ec), _)| (sr, sc, er, ec))
            .collect()
    }

    #[test]
    fn keyword_and_identifier_and_number_are_each_highlighted() {
        let ranges = kinds_at("SELECT a FROM t WHERE a = 1");
        // SELECT, a, FROM, t, WHERE, a, 1 -- seven styled tokens; `=`
        // is punctuation and is left unstyled.
        assert!(ranges.contains(&(0, 0, 0, 6)));
        assert!(ranges.contains(&(0, 26, 0, 27)));
        assert_eq!(ranges.len(), 7);
    }

    #[test]
    fn string_literal_is_highlighted_quotes_and_all() {
        let ranges = kinds_at("'hi'");
        assert_eq!(ranges, vec![(0, 0, 0, 4)]);
    }

    #[test]
    fn a_second_line_reports_row_one() {
        let ranges = kinds_at("SELECT 1\nSELECT 2");
        assert!(ranges.iter().any(|(row, ..)| *row == 1));
    }

    #[test]
    fn malformed_input_highlights_as_an_error_not_a_panic() {
        // Tokenizer never panics on bad input (db-core's own guarantee) --
        // this just confirms the highlighter doesn't defeat that by
        // unwrapping something the tokenizer left as an `Error` token.
        let ranges = highlights("`unterminated");
        assert!(!ranges.is_empty());
    }
}
