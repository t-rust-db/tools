//! The opcode-overview view (db-studio#31, `F3`): renders `Engine::
//! explain_opcodes`'s `Vec<OpcodeSection>` -- one labelled table per
//! section (row mode: a single "main" section; batch mode: one per
//! phase, e.g. build/probe/body for a join).

use db_core::engine::OpcodeSection;
use ratatui::layout::Constraint;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Row, Table};
use ratatui::Frame;

use crate::theme;

pub fn render(frame: &mut Frame, area: Rect, sections: &[OpcodeSection]) {
    let mut lines: Vec<Row> = Vec::new();
    for section in sections {
        if sections.len() > 1 {
            lines.push(Row::new(vec![Line::from(format!("[{}]", section.label))]));
        }
        for op in &section.rows {
            lines.push(Row::new(vec![
                op.addr.to_string(),
                op.opcode.clone(),
                op.operands.clone(),
            ]));
        }
    }
    let widths = [
        Constraint::Length(6),
        Constraint::Length(16),
        Constraint::Min(0),
    ];
    let header = Row::new(vec!["addr", "opcode", "operands"]).style(
        Style::default()
            .fg(theme::header())
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(lines, widths)
        .header(header)
        .block(theme::pane_block("results -- opcodes (F1 results)", false));
    frame.render_widget(table, area);
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]
mod tests {
    use super::*;
    use db_core::engine::OpcodeRow;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample() -> Vec<OpcodeSection> {
        vec![OpcodeSection {
            label: "main".to_string(),
            rows: vec![OpcodeRow {
                addr: 0,
                opcode: "Init".to_string(),
                operands: "0 3 0".to_string(),
            }],
        }]
    }

    #[test]
    fn renders_without_panicking_on_a_real_section() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &sample()))
            .unwrap();
    }

    #[test]
    fn renders_without_panicking_on_no_sections() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &[]))
            .unwrap();
    }
}
