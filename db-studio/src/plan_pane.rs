//! The query-plan view (db-studio#30, `F1`-`F4`'s `F2`): renders
//! `Engine::explain_plan`'s `PlanRow{id, parent, detail}` list as an
//! indented tree. Row and batch modes already produce the same client
//! shape (`.openspec/plan.md`'s Introspection panes section) -- this
//! pane doesn't know or care which engine produced it.

use db_core::engine::PlanRow;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::theme;

/// One line per row, indented one space per ancestor (not two -- a
/// deeply nested plan otherwise runs out of horizontal room fast).
fn lines(rows: &[PlanRow]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let depth = ancestor_depth(rows, row);
            format!("{}{}", " ".repeat(depth), row.detail)
        })
        .collect()
}

/// Renders `rows` as an indented tree, word-wrapped to the pane's width
/// -- a long plan `detail` string (a filter/join condition, typically)
/// used to run off the right edge instead of wrapping.
pub fn render(frame: &mut Frame, area: Rect, rows: &[PlanRow]) {
    let text: Vec<Line> = lines(rows).into_iter().map(Line::from).collect();
    let paragraph = Paragraph::new(text)
        .style(Style::default())
        .wrap(Wrap { trim: false })
        .block(theme::pane_block(
            "results -- query plan (F1 results)",
            false,
        ));
    frame.render_widget(paragraph, area);
}

/// Plain-text form of the same tree (db-studio#42's clipboard yank) --
/// identical content to [`render`], without the `Line`/style wrapping.
pub fn plain_text(rows: &[PlanRow]) -> String {
    lines(rows).join("\n")
}

/// How many ancestors `row` has by walking `parent` links, stopping at
/// a root (`parent == id`, batch's convention) or an unresolvable
/// parent (`parent == 0` with no matching `id`, row's convention) --
/// bounded by `rows.len()` so a malformed cycle can't loop forever.
fn ancestor_depth(rows: &[PlanRow], row: &PlanRow) -> usize {
    let mut depth = 0;
    let mut current = row;
    while depth < rows.len() {
        if current.parent == current.id {
            break;
        }
        let Some(parent) = rows.iter().find(|r| r.id == current.parent) else {
            break;
        };
        current = parent;
        depth += 1;
    }
    depth
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]
mod tests {
    use super::*;

    #[test]
    fn root_has_zero_depth_and_children_are_indented_deeper() {
        let rows = vec![
            PlanRow {
                id: 1,
                parent: 1,
                detail: "QUERY PLAN".to_string(),
            },
            PlanRow {
                id: 2,
                parent: 1,
                detail: "SCAN t".to_string(),
            },
            PlanRow {
                id: 3,
                parent: 2,
                detail: "FILTER: x > 1".to_string(),
            },
        ];
        assert_eq!(ancestor_depth(&rows, &rows[0]), 0);
        assert_eq!(ancestor_depth(&rows, &rows[1]), 1);
        assert_eq!(ancestor_depth(&rows, &rows[2]), 2);
    }

    #[test]
    fn rows_own_parent_convention_terminates_at_zero() {
        // Row mode's EqpRow-derived plans use parent == 0 for a root
        // with no id == 0 entry to match.
        let rows = vec![PlanRow {
            id: 1,
            parent: 0,
            detail: "SCAN t".to_string(),
        }];
        assert_eq!(ancestor_depth(&rows, &rows[0]), 0);
    }
}
