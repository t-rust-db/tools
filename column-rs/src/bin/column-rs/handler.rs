//! [`db_cli::ReplHandler`] impl for column-rs's `QueryEngine`, plugging the
//! `.tables`/`.schema`/`.open` dot-commands and `EXPLAIN` plan-tree
//! printing into the generic REPL loop.

use column_rs::query::{OpcodeSection, PlanNode, QueryEngine, QueryResult};
use db_cli::{render, OutputMode, ReplHandler};
use db_core::parser::Explain;
use std::path::Path;

/// A normal query result (rendered via `db_cli::render`), an `EXPLAIN
/// QUERY PLAN` plan tree, or a bare `EXPLAIN` opcode listing -- the latter
/// two always render in their own fixed format, regardless of
/// `OutputMode` (#55).
pub enum Output {
    Rows(QueryResult),
    Plan(Vec<PlanNode>),
    Opcodes(Vec<OpcodeSection>),
}

pub struct ColumnHandler {
    pub engine: QueryEngine,
}

impl ReplHandler for ColumnHandler {
    type Output = Output;

    fn execute(&mut self, input: &str) -> Result<Self::Output, String> {
        let (explain, query) = column_rs::sql::parse_explain(input).map_err(|e| e.to_string())?;
        match explain {
            Explain::QueryPlan => {
                let plan = self.engine.explain(&query).map_err(|e| e.to_string())?;
                Ok(Output::Plan(plan))
            }
            Explain::Opcodes => {
                let sections = self
                    .engine
                    .explain_opcodes(&query)
                    .map_err(|e| e.to_string())?;
                Ok(Output::Opcodes(sections))
            }
            Explain::None => self
                .engine
                .execute(input)
                .map(Output::Rows)
                .map_err(|e| e.to_string()),
        }
    }

    fn format(&self, output: &Self::Output, mode: OutputMode, headers: bool) -> String {
        match output {
            Output::Rows(result) => {
                let rows: Vec<Vec<String>> = result
                    .rows
                    .iter()
                    .map(|row| row.iter().map(|v| v.to_string()).collect())
                    .collect();
                render(mode, &result.columns, &rows, headers)
            }
            Output::Plan(nodes) => format_plan(nodes),
            Output::Opcodes(sections) => format_opcodes(sections),
        }
    }

    fn command(&mut self, name: &str, arg: &str) -> Option<Vec<String>> {
        if !name.is_empty() && "tables".starts_with(name) {
            return Some(
                self.engine
                    .table_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            );
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
            let (path_str, table_name) =
                match arg.split_once(" AS ").or_else(|| arg.split_once(" as ")) {
                    Some((path, name)) => (path.trim(), Some(name.trim().to_string())),
                    None => (arg.trim(), None),
                };
            return Some(
                match self.engine.add_table(Path::new(path_str), table_name) {
                    Ok(()) => vec![],
                    Err(e) => vec![format!("error: {e}")],
                },
            );
        }
        if !name.is_empty() && "color".starts_with(name) {
            // db-cli's generic REPL loop owns the terminal editor and does
            // not expose a hook for a handler to toggle its color output,
            // so `.color on|off` can't be wired through cleanly here.
            return Some(vec![
                "'.color' is not supported by this build's REPL".to_string()
            ]);
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
    let Some(root) = nodes.first() else {
        return out;
    };
    out.push_str(&root.detail);
    out.push('\n');
    format_children(&mut out, nodes, root.id, "");
    out
}

/// Render a bare `EXPLAIN`'s opcode listing (#55): `addr | opcode |
/// operands | comment`, one table per [`OpcodeSection`]. Multiple sections
/// (a join's `build`/`probe`/`body`) get a header line each; the
/// `Finalize` row -- the barrier between the parallel per-segment phase
/// and the sequential merge phase (ADR 0007) -- gets a separator above it
/// so the boundary is visible.
fn format_opcodes(sections: &[OpcodeSection]) -> String {
    let mut out = String::new();
    for (i, section) in sections.iter().enumerate() {
        if sections.len() > 1 {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&section.label);
            out.push('\n');
        }
        let headers = ["addr", "opcode", "operands", "comment"].map(str::to_string);
        let rows: Vec<Vec<String>> = section
            .rows
            .iter()
            .map(|r| {
                vec![
                    r.addr.to_string(),
                    r.opcode.to_string(),
                    r.operands.clone(),
                    r.comment.clone(),
                ]
            })
            .collect();

        let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.len());
            }
        }
        let push_row = |out: &mut String, cells: &[String]| {
            out.push('│');
            for (i, cell) in cells.iter().enumerate() {
                out.push_str(&format!(" {:width$} │", cell, width = widths[i]));
            }
            out.push('\n');
        };
        let push_border = |out: &mut String, left: &str, mid: &str, right: &str| {
            out.push_str(left);
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    out.push_str(mid);
                }
                out.push_str(&"─".repeat(*w + 2));
            }
            out.push_str(right);
            out.push('\n');
        };

        push_border(&mut out, "┌", "┬", "┐");
        push_row(&mut out, &headers);
        push_border(&mut out, "├", "┼", "┤");
        for (row, opcode_row) in rows.iter().zip(&section.rows) {
            // The Finalize barrier (ADR 0007): everything above ran per
            // segment in parallel, everything here runs once over the
            // merged output -- draw a divider so the boundary is visible.
            if opcode_row.is_finalize {
                push_border(&mut out, "├", "┼", "┤");
            }
            push_row(&mut out, row);
        }
        push_border(&mut out, "└", "┴", "┘");
    }
    out
}

fn format_children(out: &mut String, nodes: &[PlanNode], parent: u32, prefix: &str) {
    let children: Vec<&PlanNode> = nodes
        .iter()
        .filter(|n| n.id != n.parent && n.parent == parent)
        .collect();
    for (i, node) in children.iter().enumerate() {
        let is_last = i == children.len() - 1;
        let branch = if is_last { "└── " } else { "├── " };
        out.push_str(&format!("{prefix}{branch}{}\n", node.detail));
        let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
        format_children(out, nodes, node.id, &child_prefix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use column_rs::query::OpcodeRow;

    fn node(id: u32, parent: u32, detail: &str) -> PlanNode {
        PlanNode {
            id,
            parent,
            detail: detail.to_string(),
        }
    }

    fn opcode_row(
        addr: usize,
        opcode: &'static str,
        operands: &str,
        is_finalize: bool,
    ) -> OpcodeRow {
        OpcodeRow {
            addr,
            opcode,
            operands: operands.to_string(),
            comment: String::new(),
            is_finalize,
        }
    }

    #[test]
    fn format_opcodes_single_section_has_no_label_header() {
        let sections = vec![OpcodeSection {
            label: "body".to_string(),
            rows: vec![
                opcode_row(0, "LoadColumn", "reg=0 column=id", false),
                opcode_row(1, "Emit", "registers=[0]", false),
            ],
        }];
        let out = format_opcodes(&sections);
        assert!(!out.contains("body\n"));
        assert!(out.contains("LoadColumn"));
        assert!(out.contains("reg=0 column=id"));
    }

    #[test]
    fn format_opcodes_draws_a_divider_before_finalize() {
        let sections = vec![OpcodeSection {
            label: "body".to_string(),
            rows: vec![
                opcode_row(0, "LoadColumn", "reg=0 column=id", false),
                opcode_row(1, "Emit", "registers=[0]", false),
                opcode_row(2, "Finalize", "limit=None", true),
            ],
        }];
        let out = format_opcodes(&sections);
        // Two data borders before Finalize's row (top + the inserted
        // divider) plus the closing border: four "├" dividers total is
        // wrong to assert exactly, so just check the divider exists
        // between Emit and Finalize.
        let emit_pos = out.find("Emit").unwrap();
        let finalize_pos = out.find("Finalize").unwrap();
        let between = &out[emit_pos..finalize_pos];
        assert!(between.contains('├'));
    }

    #[test]
    fn format_opcodes_labels_multiple_sections() {
        let sections = vec![
            OpcodeSection {
                label: "JOIN build (regions)".to_string(),
                rows: vec![opcode_row(0, "HashBuild", "table=0", false)],
            },
            OpcodeSection {
                label: "JOIN probe".to_string(),
                rows: vec![opcode_row(0, "HashProbe", "table=0", false)],
            },
        ];
        let out = format_opcodes(&sections);
        assert!(out.contains("JOIN build (regions)\n"));
        assert!(out.contains("JOIN probe\n"));
    }

    #[test]
    fn empty_plan_is_empty_string() {
        assert_eq!(format_plan(&[]), "");
    }

    #[test]
    fn single_node_plan_prints_just_the_root() {
        let nodes = vec![node(0, 0, "SCAN events")];
        assert_eq!(format_plan(&nodes), "SCAN events\n");
    }

    #[test]
    fn two_level_plan_uses_last_branch_marker() {
        let nodes = vec![node(0, 0, "EMIT: region"), node(1, 0, "SCAN events")];
        let out = format_plan(&nodes);
        assert_eq!(out, "EMIT: region\n└── SCAN events\n");
    }

    #[test]
    fn multiple_children_use_middle_and_last_markers() {
        let nodes = vec![
            node(0, 0, "JOIN"),
            node(1, 0, "SCAN a"),
            node(2, 0, "SCAN b"),
        ];
        let out = format_plan(&nodes);
        assert_eq!(out, "JOIN\n├── SCAN a\n└── SCAN b\n");
    }

    #[test]
    fn three_level_nesting_indents_grandchildren() {
        let nodes = vec![
            node(0, 0, "SCAN events (3 row groups)"),
            node(1, 0, "LOAD COLUMNS: region, amount"),
        ];
        let out = format_plan(&nodes);
        assert_eq!(
            out,
            "SCAN events (3 row groups)\n└── LOAD COLUMNS: region, amount\n"
        );

        let deep = vec![
            node(0, 0, "EMIT: region"),
            node(1, 0, "SCAN events"),
            node(2, 1, "LOAD COLUMNS: region, amount"),
        ];
        let out = format_plan(&deep);
        assert_eq!(
            out,
            "EMIT: region\n└── SCAN events\n    └── LOAD COLUMNS: region, amount\n"
        );
    }
}
