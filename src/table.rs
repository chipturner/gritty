/// Format one table line: left-pad every column to its width except the last,
/// which is never padded, joining columns with two spaces.
fn format_row<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    let ncols = widths.len();
    let mut line = String::new();
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        if i + 1 < ncols {
            line.push_str(&format!("{:<width$}", cell, width = widths[i]));
        } else {
            line.push_str(cell);
        }
    }
    line
}

/// Format rows as left-aligned table lines (header first) with dynamic column
/// widths. The last column is never padded. Two spaces separate columns.
/// Returns no lines when `rows` is empty.
pub fn format_table(headers: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = headers.len();
    let widths: Vec<usize> = (0..ncols)
        .map(|i| {
            let data_max = rows.iter().map(|r| r[i].len()).max().unwrap_or(0);
            data_max.max(headers[i].len())
        })
        .collect();

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format_row(headers.iter().copied(), &widths));
    for row in rows {
        lines.push(format_row(row.iter().map(String::as_str), &widths));
    }
    lines
}

/// Print rows as a left-aligned table with dynamic column widths.
/// The last column is never padded. Two spaces separate columns.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    for line in format_table(headers, rows) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_row_pads_all_but_last() {
        let widths = [5, 3, 4];
        let row = format_row(["ab", "c", "d"].into_iter(), &widths);
        // col0 padded to 5, col1 padded to 3, last not padded; 2-space joiner.
        assert_eq!(row, "ab     c    d");
    }

    #[test]
    fn format_row_single_column_unpadded() {
        assert_eq!(format_row(["only"].into_iter(), &[10]), "only");
    }

    #[test]
    fn format_table_empty_rows_yields_no_lines() {
        assert!(format_table(&["A", "B"], &[]).is_empty());
    }

    #[test]
    fn format_table_header_then_rows() {
        let rows = vec![vec!["x".to_string(), "long-value".to_string()]];
        let lines = format_table(&["ID", "Name"], &rows);
        assert_eq!(lines, vec!["ID  Name", "x   long-value"]);
    }
}
