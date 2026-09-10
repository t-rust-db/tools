//! The file-statistics view (db-studio#32, `F4`): renders `Engine::
//! stats()`'s `FileStats` for the active file -- no query text needed,
//! always available. `Stream`'s variant is included for completeness
//! (the client type already has it), even though no engine produces it
//! yet (M5, `storage::stream` is still storage-only).

use db_core::engine::FileStats;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme;

pub fn render(frame: &mut Frame, area: Rect, stats: FileStats) {
    let lines: Vec<Line> = match stats {
        FileStats::Row {
            page_size,
            page_count,
            freelist_pages,
        } => vec![
            Line::from(format!("page size:       {page_size}")),
            Line::from(format!("page count:      {page_count}")),
            Line::from(format!("freelist pages:  {freelist_pages}")),
        ],
        FileStats::Batch { row_groups, rows } => vec![
            Line::from(format!("row groups:  {row_groups}")),
            Line::from(format!("rows:        {rows}")),
        ],
        FileStats::Stream {
            bytes_parsed,
            lines,
        } => vec![
            Line::from(format!("bytes parsed:  {bytes_parsed}")),
            Line::from(format!("lines:         {lines}")),
        ],
    };
    let paragraph = Paragraph::new(lines).block(theme::pane_block(
        "results -- file stats (F1 results)",
        false,
    ));
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[allow(
        clippy::unwrap_used,
        reason = "test code fails fast -- see db-core's own test files for the same convention"
    )]
    fn renders(stats: FileStats) {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), stats))
            .unwrap();
    }

    #[test]
    fn renders_row_stats_without_panicking() {
        renders(FileStats::Row {
            page_size: 4096,
            page_count: 10,
            freelist_pages: 1,
        });
    }

    #[test]
    fn renders_batch_stats_without_panicking() {
        renders(FileStats::Batch {
            row_groups: 2,
            rows: 1000,
        });
    }
}
