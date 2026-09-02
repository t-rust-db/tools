//! [`db_cli::ReplHandler`] impl for column-rs's `QueryEngine`, plugging the
//! `.tables`/`.schema`/`.open` dot-commands and `EXPLAIN` plan-tree
//! printing into the generic REPL loop.

use column_rs::query::{PlanNode, QueryEngine, QueryResult};
use db_cli::{render, OutputMode, ReplHandler};
use std::path::Path;

/// Either a normal query result (rendered via `db_cli::render`) or an
/// `EXPLAIN` plan tree (always tree-formatted, regardless of `OutputMode`).
pub enum Output {
    Rows(QueryResult),
    Plan(Vec<PlanNode>),
}

pub struct ColumnHandler {
    pub engine: QueryEngine,
}

impl ReplHandler for ColumnHandler {
    type Output = Output;

    fn execute(&mut self, input: &str) -> Result<Self::Output, String> {
        let (is_explain, query) = column_rs::sql::parse_explain(input).map_err(|e| e.to_string())?;
        if is_explain {
            let plan = self.engine.explain(&query).map_err(|e| e.to_string())?;
            return Ok(Output::Plan(plan));
        }
        self.engine.execute(input).map(Output::Rows).map_err(|e| e.to_string())
    }

    fn format(&self, output: &Self::Output, mode: OutputMode) -> String {
        match output {
            Output::Rows(result) => {
                let rows: Vec<Vec<String>> =
                    result.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect();
                render(mode, &result.columns, &rows)
            }
            Output::Plan(nodes) => format_plan(nodes),
        }
    }

    fn command(&mut self, name: &str, arg: &str) -> Option<Vec<String>> {
        if !name.is_empty() && "tables".starts_with(name) {
            return Some(self.engine.table_names().into_iter().map(str::to_string).collect());
        }
        if !name.is_empty() && "schema".starts_with(name) {
            let mut lines = Vec::new();
            for (table, cols) in self.engine.schemas() {
                lines.push(format!("{table}:"));
                for col in cols {
                    lines.push(format!("  {col}"));
                }
            }
            return Some(lines);
        }
        if !name.is_empty() && "open".starts_with(name) {
            if arg.is_empty() {
                return Some(vec![".open <path> [AS <name>]".to_string()]);
            }
            let (path_str, table_name) = match arg.split_once(" AS ").or_else(|| arg.split_once(" as ")) {
                Some((path, name)) => (path.trim(), Some(name.trim().to_string())),
                None => (arg.trim(), None),
            };
            return Some(match self.engine.add_table(Path::new(path_str), table_name) {
                Ok(()) => vec![],
                Err(e) => vec![format!("error: {e}")],
            });
        }
        if !name.is_empty() && "color".starts_with(name) {
            // db-cli's generic REPL loop owns the terminal editor and does
            // not expose a hook for a handler to toggle its color output,
            // so `.color on|off` can't be wired through cleanly here.
            return Some(vec!["'.color' is not supported by this build's REPL".to_string()]);
        }
        None
    }

    fn help_extra(&self) -> Vec<String> {
        vec![
            ".tables     List tables (Parquet files)".to_string(),
            ".schema     Show column schema".to_string(),
            ".open <path> [AS <name>]  Load another Parquet file into this session".to_string(),
        ]
    }

    fn banner(&self) -> Option<String> {
        Some(format!(
            "column-rs {} — Parquet analytics\nEnter SQL queries ending with `;`. Type .help for commands.\n",
            env!("CARGO_PKG_VERSION")
        ))
    }
}

/// Tree-format an `EXPLAIN` plan, e.g.:
/// ```text
/// QUERY PLAN
/// ├── SCAN events (3 row groups, ~1200000 rows)
/// │   └── LOAD COLUMNS: region, amount
/// └── EMIT: region
/// ```
fn format_plan(nodes: &[PlanNode]) -> String {
    let mut out = String::new();
    let Some(root) = nodes.first() else { return out };
    out.push_str(&root.detail);
    out.push('\n');
    format_children(&mut out, nodes, root.id, "");
    out
}

fn format_children(out: &mut String, nodes: &[PlanNode], parent: u32, prefix: &str) {
    let children: Vec<&PlanNode> = nodes.iter().filter(|n| n.id != n.parent && n.parent == parent).collect();
    for (i, node) in children.iter().enumerate() {
        let is_last = i == children.len() - 1;
        let branch = if is_last { "└── " } else { "├── " };
        out.push_str(&format!("{prefix}{branch}{}\n", node.detail));
        let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
        format_children(out, nodes, node.id, &child_prefix);
    }
}
