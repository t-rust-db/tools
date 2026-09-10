//! The left pane (db-studio#10): a schema tree, titled "Data Catalog"
//! after Harlequin's terminal SQL IDE -- the closest comparable tool,
//! and the direct source of that naming and of the table/column tree
//! shape (see `.openspec/plan.md` M1.5). Since db-studio#17 (M2), one
//! root per open file (`file -> table -> columns`) rather than a bare
//! `table -> columns` -- M1/M1.5's single-file tree is just the
//! one-root special case of this shape, not a separate code path.
//!
//! Fed by `Engine::tables()` (t-rust-db/db-core#310), not `run_query`
//! against `sqlite_master`/`PRAGMA table_info` as #10 originally
//! proposed -- neither works through `vm::row`'s compiled `SELECT` path
//! (verified, not assumed).

use crossterm::event::{KeyCode, KeyEvent};
use db_core::engine::TableInfo;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Frame;
use tui_tree_widget::{Tree, TreeItem, TreeState};

/// One open file's schema, as the tree needs it -- `key` is the file's
/// root identifier (its full path, guaranteed unique across open files,
/// unlike `label`, a bare filename that two files in different
/// directories could share), `label` what's displayed.
pub struct FileSchema {
    pub key: String,
    pub label: String,
    pub tables: Vec<TableInfo>,
}

pub struct SchemaTreePane {
    items: Vec<TreeItem<'static, String>>,
    state: TreeState<String>,
}

impl SchemaTreePane {
    pub fn new(files: Vec<FileSchema>) -> Self {
        let items = files
            .into_iter()
            .filter_map(|file| {
                let table_items = file
                    .tables
                    .into_iter()
                    .filter_map(|table| Self::table_item(table))
                    .collect();
                // Unreachable in practice -- `key` is a file path, and
                // two open files never share one.
                TreeItem::new(file.key, file.label, table_items).ok()
            })
            .collect();
        Self {
            items,
            state: TreeState::default(),
        }
    }

    fn table_item(table: TableInfo) -> Option<TreeItem<'static, String>> {
        let children = table
            .columns
            .into_iter()
            .map(|col| {
                let label = if col.type_name.is_empty() {
                    col.name.clone()
                } else {
                    format!("{} ({})", col.name, col.type_name)
                };
                TreeItem::new_leaf(col.name, label)
            })
            .collect();
        // SQLite guarantees unique column names within one table, so
        // this never actually fails -- a malformed schema drops the
        // table from the tree rather than crashing the whole pane over
        // it.
        TreeItem::new(table.name.clone(), table.name, children).ok()
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left => {
                self.state.key_left();
            }
            KeyCode::Right => {
                self.state.key_right();
            }
            KeyCode::Down => {
                self.state.key_down();
            }
            KeyCode::Up => {
                self.state.key_up();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.state.toggle_selected();
            }
            _ => {}
        }
    }

    /// The selected node's file-root key, when the current selection
    /// *is* a file root (a one-element selection path) rather than a
    /// table or column nested under one. `db-studio#18` uses this to
    /// tell "switch the active file" apart from "just expanding a
    /// table."
    pub fn selected_file_key(&self) -> Option<&str> {
        match self.state.selected() {
            [key] => Some(key.as_str()),
            _ => None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let Ok(tree) = Tree::new(&self.items) else {
            // Unreachable in practice -- file paths come from argv,
            // which never has one repeated to two different OpenFiles.
            return;
        };
        let tree = tree
            .block(crate::theme::pane_block("Data Catalog", focused))
            .highlight_style(Style::default().bg(crate::theme::selection_bg()));
        frame.render_stateful_widget(tree, area, &mut self.state);
        // `TreeState::select_first` only has something to select once a
        // render has populated its internal flattened-item cache -- it's
        // a no-op before the first render, so this can't move to `new`.
        // Guarded on an empty selection so it doesn't fight the user's
        // own navigation on every subsequent frame.
        if self.state.selected().is_empty() {
            self.state.select_first();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::panic,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]
mod tests {
    use super::*;
    use db_core::engine::ColumnInfo;

    fn sample_table() -> TableInfo {
        TableInfo {
            name: "items".to_string(),
            columns: vec![
                ColumnInfo {
                    name: "id".to_string(),
                    type_name: "INTEGER".to_string(),
                },
                ColumnInfo {
                    name: "name".to_string(),
                    type_name: "TEXT".to_string(),
                },
            ],
        }
    }

    fn one_file(key: &str) -> FileSchema {
        FileSchema {
            key: key.to_string(),
            label: key.to_string(),
            tables: vec![sample_table()],
        }
    }

    #[test]
    fn one_file_builds_a_single_root_with_its_tables_as_children() {
        let pane = SchemaTreePane::new(vec![one_file("a.sqlite")]);
        assert_eq!(pane.items.len(), 1);
        assert_eq!(pane.items[0].children().len(), 1);
        let debug = format!("{:?}", pane.items[0]);
        assert!(debug.contains("items"), "{debug}");
    }

    #[test]
    fn multiple_files_are_sibling_roots() {
        let pane = SchemaTreePane::new(vec![one_file("a.sqlite"), one_file("b.sqlite")]);
        assert_eq!(pane.items.len(), 2);
    }

    #[test]
    fn duplicate_table_names_across_different_files_do_not_collide() {
        // Table identifiers only need to be unique among siblings --
        // two files can each have their own "items" table.
        let pane = SchemaTreePane::new(vec![one_file("a.sqlite"), one_file("b.sqlite")]);
        assert_eq!(pane.items.len(), 2);
        assert_eq!(pane.items[0].children().len(), 1);
        assert_eq!(pane.items[1].children().len(), 1);
    }

    #[test]
    fn selected_file_key_is_none_before_any_selection() {
        let pane = SchemaTreePane::new(vec![one_file("a.sqlite")]);
        assert_eq!(pane.selected_file_key(), None);
    }
}
