/// Terminal columns a cell occupies.
///
/// Not `str::len()` (bytes) and not `chars().count()`: `日` is 3 bytes, 1 char,
/// and 2 columns. Measuring in either of the other two units misaligns any table
/// holding CJK or emoji -- both reachable here, since the `Cmd` and `CWD` columns
/// are user-controlled.
///
/// Assumes the cell holds no ANSI escapes; callers style whole lines, never
/// cells (see `print_session_table`).
fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Format one table line: pad every column out to its width except the last,
/// which is never padded, joining columns with two spaces.
fn format_row<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    let ncols = widths.len();
    let mut line = String::new();
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if i + 1 < ncols {
            // Padded by hand: `{:<width$}` pads to a *char* count, which is a
            // third unit again and would undo `display_width`'s work.
            let pad = widths[i].saturating_sub(display_width(cell));
            line.extend(std::iter::repeat_n(' ', pad));
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
            let data_max = rows.iter().map(|r| display_width(&r[i])).max().unwrap_or(0);
            data_max.max(display_width(headers[i]))
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

    /// Display columns before `last` begins. Columns line up iff this matches
    /// across rows.
    fn prefix_width(line: &str, last: &str) -> usize {
        let idx = line.rfind(last).unwrap_or_else(|| panic!("{last:?} not in {line:?}"));
        display_width(&line[..idx])
    }

    /// `日` is 3 bytes, 1 char, and 2 columns. Measuring bytes and padding with
    /// `{:<w$}` (which counts chars) got both units wrong.
    #[test]
    fn wide_characters_align_by_display_column() {
        let rows = vec![
            vec!["/srv/日本語".to_string(), "A".to_string()],
            vec!["/srv/abcdef".to_string(), "B".to_string()],
        ];
        let lines = format_table(&["CWD", "S"], &rows);
        assert_eq!(
            prefix_width(&lines[1], "A"),
            prefix_width(&lines[2], "B"),
            "\n{}\n{}",
            lines[1],
            lines[2]
        );
    }

    #[test]
    fn emoji_align_by_display_column() {
        let rows = vec![
            vec!["/srv/🚀x".to_string(), "A".to_string()],
            vec!["/srv/abc".to_string(), "B".to_string()],
        ];
        let lines = format_table(&["CWD", "S"], &rows);
        assert_eq!(
            prefix_width(&lines[1], "A"),
            prefix_width(&lines[2], "B"),
            "\n{}\n{}",
            lines[1],
            lines[2]
        );
    }

    /// Accented Latin used to align *by accident*: the byte-width overshoot and
    /// the char-count padding cancelled. Keep it aligned, now on purpose, and
    /// without the phantom extra column the overshoot produced.
    #[test]
    fn accented_latin_aligns_without_overshoot() {
        let rows = vec![
            vec!["café".to_string(), "A".to_string()],
            vec!["abcd".to_string(), "B".to_string()],
        ];
        let lines = format_table(&["CWD", "S"], &rows);
        assert_eq!(prefix_width(&lines[1], "A"), prefix_width(&lines[2], "B"));
        assert_eq!(lines[1], "café  A");
        assert_eq!(lines[2], "abcd  B");
    }

    /// A cell wider than its header still pads the header out to the column.
    #[test]
    fn header_narrower_than_wide_cell() {
        let rows = vec![vec!["日本".to_string(), "A".to_string()]];
        let lines = format_table(&["C", "S"], &rows);
        assert_eq!(prefix_width(&lines[0], "S"), prefix_width(&lines[1], "A"));
    }

    #[test]
    fn display_width_counts_columns_not_bytes_or_chars() {
        assert_eq!(display_width("café"), 4); // 5 bytes, 4 chars, 4 columns
        assert_eq!(display_width("日本語"), 6); // 9 bytes, 3 chars, 6 columns
        assert_eq!(display_width("plain"), 5);
    }

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
