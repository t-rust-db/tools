//! Generic table/JSON/CSV rendering over plain string rows and headers.
//! No engine-specific value types — callers stringify their own cell values
//! before calling in.

/// Which format a [`crate::ReplHandler`] should render its output as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Table,
    Json,
    Csv,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Some(OutputMode::Table),
            "json" => Some(OutputMode::Json),
            "csv" => Some(OutputMode::Csv),
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

/// RFC 4180-ish CSV: quotes a field if it contains a comma, quote, or newline.
pub fn render_csv(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str(&csv_row(headers));
    out.push('\n');
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

/// Render headers/rows using the given [`OutputMode`].
pub fn render(mode: OutputMode, headers: &[String], rows: &[Vec<String>]) -> String {
    match mode {
        OutputMode::Table => render_table(headers, rows),
        OutputMode::Json => render_json(headers, rows),
        OutputMode::Csv => render_csv(headers, rows),
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
        let out = render_csv(&headers, &rows);
        assert!(out.contains("\"b, c\""));
        assert_eq!(out.lines().next().unwrap(), "id,name");
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
}
