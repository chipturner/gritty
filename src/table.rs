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

    // Print header
    let mut line = String::new();
    for (i, hdr) in headers.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        if i < ncols - 1 {
            line.push_str(&format!("{:<width$}", hdr, width = widths[i]));
        } else {
            line.push_str(hdr);
        }
    }
    println!("{line}");

    // Print rows
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            if i < ncols - 1 {
                line.push_str(&format!("{:<width$}", cell, width = widths[i]));
            } else {
                line.push_str(cell);
            }
        }
        println!("{line}");
    }
}
