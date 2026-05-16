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

/// Print rows as a left-aligned table with dynamic column widths.
/// The last column is never padded. Two spaces separate columns.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let ncols = headers.len();
    let widths: Vec<usize> = (0..ncols)
        .map(|i| {
            let data_max = rows.iter().map(|r| r[i].len()).max().unwrap_or(0);
            data_max.max(headers[i].len())
        })
        .collect();

    println!("{}", format_row(headers.iter().copied(), &widths));
    for row in rows {
        println!("{}", format_row(row.iter().map(String::as_str), &widths));
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
}
