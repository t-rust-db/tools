//! Generic table/JSON/CSV/list/column/line rendering over plain string rows
//! and headers. No engine-specific value types — callers stringify their
//! own cell values before calling in.

use std::fmt::Write as _;

/// Which format a [`crate::ReplHandler`] should render its output as.
/// `List`/`Column`/`Line`/`Csv` match `sqlite3`'s own `.mode` set (#6);
/// `Table`/`Json` are db-cli's own additions with no `sqlite3` precedent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Table,
    List,
    Column,
    Line,
    Csv,
    Json,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Some(OutputMode::Table),
            "list" => Some(OutputMode::List),
            "column" => Some(OutputMode::Column),
            "line" => Some(OutputMode::Line),
            "csv" => Some(OutputMode::Csv),
            "json" => Some(OutputMode::Json),
            _ => None,
        }
    }
}

/// ASCII box-drawn table, e.g.:
/// ```text
/// ┌────┬─────┐
/// │ id │ val │
/// ├────┼─────┤
/// │ 1  │ a   │
/// └────┴─────┘
/// ```
/// db-cli's own addition (no `sqlite3` equivalent) -- always shows headers,
/// unaffected by `.headers off`.
pub fn render_table(headers: &[String], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let mut out = String::new();

    let border = |out: &mut String, left: &str, mid: &str, right: &str| {
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

    border(&mut out, "┌", "┬", "┐");

    out.push('│');
    for (i, h) in headers.iter().enumerate() {
        out.push_str(&format!(" {:width$} │", h, width = widths[i]));
    }
    out.push('\n');

    border(&mut out, "├", "┼", "┤");

    for row in rows {
        out.push('│');
        for (i, cell) in row.iter().enumerate() {
            let w = widths.get(i).copied().unwrap_or(cell.len());
            out.push_str(&format!(" {:width$} │", cell, width = w));
        }
        out.push('\n');
    }

    border(&mut out, "└", "┴", "┘");

    out
}

/// One JSON array of objects, headers as keys, all values as JSON strings.
/// db-cli's own addition -- always shows headers (a JSON object is
/// self-describing), unaffected by `.headers off`.
pub fn render_json(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::from("[\n");
    for (ri, row) in rows.iter().enumerate() {
        out.push_str("  {");
        for (i, h) in headers.iter().enumerate() {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{}: {}", json_string(h), json_string(cell)));
        }
        out.push('}');
        if ri + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// RFC 4180-ish CSV: quotes a field if it contains a comma, quote, or
/// newline. `show_headers` matches `sqlite3`'s `.headers` toggle.
pub fn render_csv(headers: &[String], rows: &[Vec<String>], show_headers: bool) -> String {
    let mut out = String::new();
    if show_headers {
        out.push_str(&csv_row(headers));
        out.push('\n');
    }
    for row in rows {
        out.push_str(&csv_row(row));
        out.push('\n');
    }
    out
}

fn csv_row(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",")
}

fn csv_field(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// `sqlite3`'s default `.mode list`: fields joined with `|`, one row per
/// line. `show_headers` matches `.headers`.
pub fn render_list(headers: &[String], rows: &[Vec<String>], show_headers: bool) -> String {
    let mut out = String::new();
    if show_headers {
        out.push_str(&headers.join("|"));
        out.push('\n');
    }
    for row in rows {
        out.push_str(&row.join("|"));
        out.push('\n');
    }
    out
}

/// `sqlite3`'s `.mode column`: fixed-width columns, each sized to its
/// longest rendered value (header included, when shown), separated by a
/// 2-space gap; the last column isn't padded. When `show_headers` is set,
/// a header row is followed by a `-`-filled row of the same widths. This
/// is a reasonable approximation of `sqlite3`'s own auto-sizing (which
/// samples row data with its own heuristics), not a byte-exact algorithmic
/// match -- widths are computed from every row, and there's no
/// truncation.
pub fn render_column(headers: &[String], rows: &[Vec<String>], show_headers: bool) -> String {
    let num_cols = headers.len();
    let mut widths = vec![0usize; num_cols];
    if show_headers {
        for (w, h) in widths.iter_mut().zip(headers.iter()) {
            *w = (*w).max(h.len());
        }
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.len());
            }
        }
    }

    let write_row = |out: &mut String, cells: &[String]| {
        for (i, cell) in cells.iter().enumerate() {
            let width = widths.get(i).copied().unwrap_or(0);
            if i.saturating_add(1) == num_cols {
                out.push_str(cell);
            } else {
                let _ = write!(out, "{cell:<width$}  ");
            }
        }
        out.push('\n');
    };

    let mut out = String::new();
    if show_headers {
        write_row(&mut out, headers);
        let dashes: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        write_row(&mut out, &dashes);
    }
    for row in rows {
        write_row(&mut out, row);
    }
    out
}

/// `sqlite3`'s `.mode line`: one `column_name = value` line per column, a
/// blank line between rows. Not gated by `.headers` -- `.mode line` always
/// labels every value with its column name, matching stock `sqlite3`.
pub fn render_line(headers: &[String], rows: &[Vec<String>]) -> String {
    let name_width = headers.iter().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (row_i, row) in rows.iter().enumerate() {
        if row_i > 0 {
            out.push('\n');
        }
        for (i, value) in row.iter().enumerate() {
            let name = headers.get(i).map(String::as_str).unwrap_or("");
            let _ = writeln!(out, "{name:<name_width$} = {value}");
        }
    }
    out
}

/// Render headers/rows using the given [`OutputMode`]. `show_headers`
/// matches `sqlite3`'s `.headers` toggle for `List`/`Column`/`Csv`;
/// `Table`/`Json` always show headers (no `sqlite3` precedent to match),
/// `Line` never gates on it (see [`render_line`]).
pub fn render(
    mode: OutputMode,
    headers: &[String],
    rows: &[Vec<String>],
    show_headers: bool,
) -> String {
    match mode {
        OutputMode::Table => render_table(headers, rows),
        OutputMode::Json => render_json(headers, rows),
        OutputMode::Csv => render_csv(headers, rows, show_headers),
        OutputMode::List => render_list(headers, rows, show_headers),
        OutputMode::Column => render_column(headers, rows, show_headers),
        OutputMode::Line => render_line(headers, rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vec<String>, Vec<Vec<String>>) {
        (
            vec!["id".to_string(), "name".to_string()],
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b, c".to_string()],
            ],
        )
    }

    #[test]
    fn table_has_header_and_row_count() {
        let (headers, rows) = sample();
        let out = render_table(&headers, &rows);
        // top border, header, separator, 2 data rows, bottom border.
        assert_eq!(out.lines().count(), 6);
    }

    #[test]
    fn json_round_trips_field_count() {
        let (headers, rows) = sample();
        let out = render_json(&headers, &rows);
        assert!(out.contains("\"id\": \"1\""));
        assert!(out.contains("\"name\": \"b, c\""));
    }

    #[test]
    fn csv_quotes_fields_with_commas() {
        let (headers, rows) = sample();
        let out = render_csv(&headers, &rows, true);
        assert!(out.contains("\"b, c\""));
        assert_eq!(out.lines().next().unwrap(), "id,name");
    }

    #[test]
    fn csv_omits_header_row_when_show_headers_is_false() {
        let (headers, rows) = sample();
        let out = render_csv(&headers, &rows, false);
        assert_eq!(out, "1,a\n2,\"b, c\"\n");
    }

    #[test]
    fn empty_headers_render_empty_table() {
        assert_eq!(render_table(&[], &[]), "");
    }

    #[test]
    fn mode_parse_is_case_insensitive() {
        assert_eq!(OutputMode::parse("JSON"), Some(OutputMode::Json));
        assert_eq!(OutputMode::parse("nope"), None);
    }

    #[test]
    fn mode_parse_covers_sqlite3_modes() {
        assert_eq!(OutputMode::parse("list"), Some(OutputMode::List));
        assert_eq!(OutputMode::parse("COLUMN"), Some(OutputMode::Column));
        assert_eq!(OutputMode::parse("Line"), Some(OutputMode::Line));
    }

    #[test]
    fn list_mode_is_pipe_separated() {
        let (headers, rows) = sample();
        assert_eq!(render_list(&headers, &rows, false), "1|a\n2|b, c\n");
        assert_eq!(render_list(&headers, &rows, true), "id|name\n1|a\n2|b, c\n");
    }

    #[test]
    fn column_mode_pads_to_widest_value_with_two_space_gap() {
        let (headers, rows) = sample();
        assert_eq!(
            render_column(&headers, &rows, true),
            "id  name\n--  ----\n1   a\n2   b, c\n"
        );
        // Without headers, widths come from row data alone (id is 1 wide,
        // not 2), so column widths shrink compared to the headers-on case.
        assert_eq!(render_column(&headers, &rows, false), "1  a\n2  b, c\n");
    }

    #[test]
    fn line_mode_labels_every_value_with_a_blank_line_between_rows() {
        let (headers, rows) = sample();
        assert_eq!(
            render_line(&headers, &rows),
            "id   = 1\nname = a\n\nid   = 2\nname = b, c\n"
        );
    }

    #[test]
    fn render_dispatches_by_mode() {
        let (headers, rows) = sample();
        assert_eq!(
            render(OutputMode::List, &headers, &rows, false),
            render_list(&headers, &rows, false)
        );
        assert_eq!(
            render(OutputMode::Column, &headers, &rows, true),
            render_column(&headers, &rows, true)
        );
        assert_eq!(
            render(OutputMode::Line, &headers, &rows, true),
            render_line(&headers, &rows)
        );
    }
}
