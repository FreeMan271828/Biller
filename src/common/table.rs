//! 极简终端表格渲染：按显示宽度对齐，中文按两列宽计算。

/// 单个字符占用的终端列数（只处理常见全角区间）。
fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// 字符串在终端里占用的列数。
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 右侧补空格到指定显示宽度。
pub fn pad_right(text: &str, width: usize) -> String {
    let current = display_width(text);
    if current >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - current))
    }
}

/// 打印一张表；`rows` 为空时打印提示而不是空白。
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(display_width(cell));
        }
    }

    let render = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| pad_right(cell, widths[index]))
            .collect::<Vec<_>>()
            .join("  ")
    };

    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", render(&header_cells));
    println!(
        "{}",
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("  ")
    );

    if rows.is_empty() {
        println!("(无数据)");
        return;
    }
    for row in rows {
        println!("{}", render(row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_cjk_as_double_width() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("餐饮"), 4);
        assert_eq!(display_width("a餐"), 3);
        assert_eq!(pad_right("餐", 4), "餐  ");
    }
}
