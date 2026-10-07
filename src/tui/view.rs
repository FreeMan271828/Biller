//! ratatui 渲染层：把 [`App`] 的状态画成一屏页面。
//!
//! 布局自上而下：页签栏 / 页面内容 / 状态栏 + 快捷键，弹窗叠加在最上层。

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

use crate::common::{money, time};
use crate::tui::app::{App, Form, Modal, Page};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(frame.area());

    render_tabs(frame, chunks[0], app);
    render_body(frame, chunks[1], app);
    render_footer(frame, chunks[2], app);

    if app.modal.is_some() {
        render_modal(frame, app);
    }
}

fn render_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles = Page::ALL
        .iter()
        .map(|page| Line::from(format!(" {} ", page.title())));

    let block = Block::bordered().title(Line::from(vec![
        Span::styled(
            " Biller 账单记账 ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", time::format_month(app.month)),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(" [ / ] 换月 ", Style::default().fg(MUTED)),
    ]));

    let tabs = Tabs::new(titles)
        .select(app.page.index())
        .block(block)
        .divider(" ")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(tabs, area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    match app.page {
        Page::Overview => render_overview(frame, area, app),
        Page::Bills => render_bills(frame, area, app),
        Page::Books => render_books(frame, area, app),
        Page::Categories => render_categories(frame, area, app),
        Page::Months => render_months(frame, area, app),
        Page::Years => render_years(frame, area, app),
    }
}

fn render_overview(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.report else {
        frame.render_widget(Paragraph::new("加载中…"), area);
        return;
    };

    let chunks = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    let collection = &report.collection;

    let lines = vec![
        Line::from(""),
        kv("起始金额", collection.start_balance, Color::White),
        kv("收入合计", collection.total_income, Color::Green),
        kv("支出合计", collection.total_expense, Color::Red),
        kv("本月净额", collection.net(), Color::Yellow),
        kv("月末结余", collection.remaining(), ACCENT),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" 本月概览 · 按 e 改起始金额 ")),
        chunks[0],
    );

    render_group(
        frame,
        chunks[1],
        "支出构成（按分类）",
        &collection.expense_by_category,
        &report.category_names,
        Color::Red,
    );
}

fn render_months(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.report else {
        frame.render_widget(Paragraph::new("加载中…"), area);
        return;
    };
    let collection = &report.collection;

    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Min(5),
    ])
    .split(area);

    // 顶部：本月汇总（月报只按分类口径，账本口径在年报）
    let summary = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    frame.render_widget(
        Paragraph::new(vec![
            kv("起始金额", collection.start_balance, Color::White),
            kv("收入合计", collection.total_income, Color::Green),
        ])
        .block(Block::bordered().title(format!(
            " {} 汇总 · [ ] 换月 ",
            time::format_month(collection.month)
        ))),
        summary[0],
    );
    frame.render_widget(
        Paragraph::new(vec![
            kv("支出合计", collection.total_expense, Color::Red),
            kv("本月结余", collection.remaining(), ACCENT),
        ])
        .block(Block::bordered()),
        summary[1],
    );

    // 中部：按分类支出（可选） + 选中分类的明细
    let middle = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(rows[1]);
    render_month_expense_categories(frame, middle[0], app);
    render_month_category_detail(frame, middle[1], app);

    // 底部：按分类收入
    render_group(
        frame,
        rows[2],
        "按分类 · 收入",
        &collection.income_by_category,
        &report.category_names,
        Color::Green,
    );
}

fn render_month_expense_categories(frame: &mut Frame, area: Rect, app: &App) {
    let entries = app.month_expense_categories();
    let selected = app.selected_index(Page::Months);
    let names = app
        .report
        .as_ref()
        .map(|report| report.category_names.clone())
        .unwrap_or_default();

    let rows = entries
        .iter()
        .enumerate()
        .map(|(index, (id, amount))| {
            Row::new(vec![
                Cell::from(
                    names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("(已删除 id={id})")),
                ),
                Cell::from(money::format_amount(*amount)),
            ])
            .style(highlight(index == selected))
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [Constraint::Min(8), Constraint::Length(12)])
        .header(header(["分类", "支出"]))
        .column_spacing(1)
        .block(Block::bordered().title(Line::from(Span::styled(
            " 按分类 · 支出（↑↓ 选分类看明细） ",
            Style::default().fg(Color::Red),
        ))));

    frame.render_widget(table, area);
}

fn render_month_category_detail(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.month_selected_category_name() {
        Some(name) => format!(" 明细 · {name} "),
        None => " 明细 ".to_string(),
    };

    let rows = app
        .month_category_bills()
        .iter()
        .map(|bill| {
            Row::new(vec![
                Cell::from(bill.id.to_string()),
                Cell::from(time::format_date(bill.created_at)),
                Cell::from(money::format_amount(bill.amount)),
                Cell::from(bill.book.name.clone()),
                Cell::from(bill.remark.clone().unwrap_or_default()),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(6),
        ],
    )
    .header(header(["ID", "日期", "金额", "账本", "备注"]))
    .column_spacing(1)
    .block(Block::bordered().title(title));

    frame.render_widget(table, area);
}

fn render_years(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.year_report else {
        frame.render_widget(Paragraph::new("加载中…"), area);
        return;
    };
    let collection = &report.collection;

    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Min(6),
    ])
    .split(area);

    let summary = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    frame.render_widget(
        Paragraph::new(vec![
            kv("收入合计", collection.total_income, Color::Green),
            kv("支出合计", collection.total_expense, Color::Red),
        ])
        .block(Block::bordered().title(format!(
            " {} 年报 · [ ] 换年 ",
            collection.year
        ))),
        summary[0],
    );
    frame.render_widget(
        Paragraph::new(vec![
            kv("全年净额", collection.net(), Color::Yellow),
            Line::from(Span::styled(
                "  年内分账本，逐年看走势",
                Style::default().fg(MUTED),
            )),
        ])
        .block(Block::bordered()),
        summary[1],
    );

    // 年报的口径是账本
    let books = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    render_group(
        frame,
        books[0],
        "按账本 · 收入",
        &collection.income_by_book,
        &report.book_names,
        Color::Green,
    );
    render_group(
        frame,
        books[1],
        "按账本 · 支出",
        &collection.expense_by_book,
        &report.book_names,
        Color::Red,
    );

    let monthly = (0..12)
        .map(|index| {
            let income = collection.income_by_month[index];
            let expense = collection.expense_by_month[index];
            Row::new(vec![
                Cell::from(format!("{} 月", index + 1)),
                Cell::from(money::format_amount(income)),
                Cell::from(money::format_amount(expense)),
                Cell::from(money::format_amount(income - expense)),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        monthly,
        [
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(14),
        ],
    )
    .header(header(["月份", "收入", "支出", "净额"]))
    .column_spacing(1)
    .block(Block::bordered().title(" 逐月走势 "));

    frame.render_widget(table, rows[2]);
}

fn render_bills(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.selected_index(Page::Bills);
    let rows = app
        .bills
        .iter()
        .enumerate()
        .map(|(index, bill)| {
            Row::new(vec![
                Cell::from(bill.id.to_string()),
                Cell::from(time::format_date(bill.created_at)),
                Cell::from(bill.kind.to_string()),
                Cell::from(money::format_amount(bill.amount)),
                Cell::from(bill.book.name.clone()),
                Cell::from(bill.category.name.clone()),
                Cell::from(bill.remark.clone().unwrap_or_default()),
            ])
            .style(highlight(index == selected))
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(8),
        ],
    )
    .header(header(["ID", "日期", "方向", "金额", "账本", "分类", "备注"]))
    .column_spacing(1)
    .block(Block::bordered().title(format!(
        " {} 账单 · 共 {} 笔 ",
        time::format_month(app.month),
        app.bills.len()
    )));

    frame.render_widget(table, area);
}

fn render_books(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.selected_index(Page::Books);
    let rows = app
        .books
        .iter()
        .enumerate()
        .map(|(index, book)| {
            Row::new(vec![
                Cell::from(book.id.to_string()),
                Cell::from(book.name.clone()),
            ])
            .style(highlight(index == selected))
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [Constraint::Length(6), Constraint::Min(12)])
        .header(header(["ID", "账本"]))
        .column_spacing(1)
        .block(Block::bordered().title(format!(" 账本 · 共 {} 个 ", app.books.len())));

    frame.render_widget(table, area);
}

fn render_categories(frame: &mut Frame, area: Rect, app: &App) {
    let names: HashMap<u64, String> = app
        .categories
        .iter()
        .map(|node| (node.category.id, node.category.name.clone()))
        .collect();
    let selected = app.selected_index(Page::Categories);

    let rows = app
        .categories
        .iter()
        .enumerate()
        .map(|(index, node)| {
            // 树状缩进：子分类用 └─ 前缀体现层级
            let indent = if node.depth == 0 {
                String::new()
            } else {
                format!("{}└─ ", "  ".repeat(node.depth - 1))
            };
            let parent = match node.category.parent_id {
                Some(id) => names.get(&id).cloned().unwrap_or_else(|| format!("id={id}")),
                None => "-".to_string(),
            };
            Row::new(vec![
                Cell::from(node.category.id.to_string()),
                Cell::from(format!("{indent}{}", node.category.name)),
                Cell::from(parent),
            ])
            .style(highlight(index == selected))
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Min(12),
            Constraint::Length(14),
        ],
    )
    .header(header(["ID", "分类", "父分类"]))
    .column_spacing(1)
    .block(Block::bordered().title(format!(
        " 分类 · 共 {} 个 ",
        app.categories.len()
    )));

    frame.render_widget(table, area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    let status = match &app.status {
        Some(status) if status.is_error() => Line::from(Span::styled(
            format!("✖ {}", status.text()),
            Style::default().fg(Color::Red),
        )),
        Some(status) => Line::from(Span::styled(
            format!("✔ {}", status.text()),
            Style::default().fg(Color::Green),
        )),
        None => Line::from(Span::styled("就绪", Style::default().fg(MUTED))),
    };
    frame.render_widget(Paragraph::new(status), chunks[0]);

    let help = match &app.modal {
        Some(Modal::Picker { .. }) => "↑↓ 选择    Enter 确定    Esc 取消",
        Some(_) => "Tab/↑↓ 切字段    ←/→ 或空格 切选项    Enter 展开选项/提交    Esc 关闭",
        None => {
            "Tab/←→/1-6 切页    ↑↓ 选择    [ ] 换月/换年    a 新增    e 编辑    d 删除    r 刷新    m 本月    q 退出"
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(help, Style::default().fg(MUTED)))),
        chunks[1],
    );
}

fn render_modal(frame: &mut Frame, app: &App) {
    let Some(modal) = &app.modal else {
        return;
    };
    let area = centered(frame.area(), 62, 60);
    frame.render_widget(Clear, area);

    match modal {
        Modal::Form(form) => render_form(frame, area, form),
        Modal::Picker {
            label,
            options,
            selected,
            ..
        } => render_picker(frame, label, options, *selected),
        Modal::Confirm { title, message, .. } => render_message(
            frame,
            area,
            title,
            message,
            "Enter / y 确认        Esc / n 取消",
            Color::Yellow,
        ),
        Modal::Notice { title, message } => {
            render_message(frame, area, title, message, "按任意键关闭", Color::Red)
        }
    }
}

/// 展开的选择列表：只渲染能容纳的一段，并保证当前项始终可见。
fn render_picker(frame: &mut Frame, label: &str, options: &[String], selected: usize) {
    let area = centered_rows(frame.area(), 46, options.len() as u16 + 4);
    frame.render_widget(Clear, area);

    let visible = area.height.saturating_sub(2) as usize;
    let start = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };

    let rows = options
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, option)| {
            Row::new(vec![Cell::from(option.clone())]).style(highlight(index == selected))
        })
        .collect::<Vec<_>>();

    let position = if options.is_empty() {
        " 没有可选项 ".to_string()
    } else {
        format!(
            " 第 {}/{} 项 · ↑↓ 选择  Enter 确定  Esc 取消 ",
            selected + 1,
            options.len()
        )
    };

    let table = Table::new(rows, [Constraint::Min(10)]).block(
        Block::bordered()
            .title(Line::from(Span::styled(
                format!(" 选择{label} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )))
            .title_bottom(Line::from(Span::styled(position, Style::default().fg(MUTED)))),
    );

    frame.render_widget(table, area);
}

/// 固定行数、按宽度百分比居中的弹窗区域。
fn centered_rows(area: Rect, percent_x: u16, height: u16) -> Rect {
    let height = height.min(area.height);
    let width = (area.width * percent_x / 100).max(20).min(area.width);
    let top = area.y + area.height.saturating_sub(height) / 2;
    let left = area.x + area.width.saturating_sub(width) / 2;
    Rect::new(left, top, width, height)
}

fn render_form(frame: &mut Frame, area: Rect, form: &Form) {
    let mut lines: Vec<Line> = vec![Line::from("")];

    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        let marker = if focused { "▶ " } else { "  " };
        let label_style = if focused {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(Color::Gray)
        };
        let value_style = if focused {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(Color::White)
        };

        // 选择框用 ▾ 标出来，一眼能看出这行能不能按 Enter 展开
        let expandable = if field.is_choice() { " ▾" } else { "" };

        lines.push(Line::from(vec![
            Span::styled(format!("  {marker}{:<8}", field.label), label_style),
            Span::styled(format!(" {} ", field.value), value_style),
            Span::styled(expandable, Style::default().fg(MUTED)),
        ]));
    }

    lines.push(Line::from(""));
    if let Some(field) = form.current() {
        lines.push(Line::from(Span::styled(
            format!("  提示：{}", field.hint),
            Style::default().fg(MUTED),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  Enter 提交（选择框上为展开列表）    Esc 取消",
        Style::default().fg(MUTED),
    )));

    let block = Block::bordered().title(Line::from(Span::styled(
        format!(" {} ", form.title),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_message(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    message: &str,
    hint: &str,
    color: Color,
) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(format!("  {hint}"), Style::default().fg(MUTED))),
    ];

    let block = Block::bordered().title(Line::from(Span::styled(
        format!(" {title} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_group(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    values: &HashMap<u64, i64>,
    names: &HashMap<u64, String>,
    color: Color,
) {
    let mut entries: Vec<(u64, i64)> = values.iter().map(|(id, amount)| (*id, *amount)).collect();
    entries.sort_by(|left, right| right.1.cmp(&left.1));

    let rows = entries
        .iter()
        .map(|(id, amount)| {
            Row::new(vec![
                Cell::from(
                    names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("(已删除 id={id})")),
                ),
                Cell::from(money::format_amount(*amount)),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [Constraint::Min(8), Constraint::Length(12)])
        .header(header(["名称", "金额"]))
        .column_spacing(1)
        .block(Block::bordered().title(Line::from(Span::styled(
            format!(" {title} "),
            Style::default().fg(color),
        ))));

    frame.render_widget(table, area);
}

fn header<const N: usize>(titles: [&'static str; N]) -> Row<'static> {
    Row::new(titles)
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
}

fn highlight(selected: bool) -> Style {
    if selected {
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn kv(label: &str, amount: i64, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<10}"), Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:>12} 元", money::format_amount(amount)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// 居中的弹窗区域（按百分比）。
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bill::model::{BillEntity, BillKind};
    use crate::bill_book::model::BillBook;
    use crate::category::model::Category;
    use crate::category::service::CategoryNode;
    use crate::month::model::{BillMonthCollection, YearCollection};
    use crate::month::service::{MonthReport, YearReport};
    use crate::tui::app::{Action, Form, Modal};
    use chrono::{NaiveDate, TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_app() -> App {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut app = App::new(month);

        app.books = vec![
            BillBook {
                id: 1,
                name: "微信".into(),
            },
            BillBook {
                id: 2,
                name: "支付宝".into(),
            },
        ];
        app.categories = vec![
            CategoryNode {
                depth: 0,
                category: Category {
                    id: 1,
                    parent_id: None,
                    name: "餐饮".into(),
                },
            },
            CategoryNode {
                depth: 1,
                category: Category {
                    id: 2,
                    parent_id: Some(1),
                    name: "午餐".into(),
                },
            },
        ];
        app.bills = vec![BillEntity {
            id: 7,
            kind: BillKind::Expense,
            amount: 1234,
            book: BillBook {
                id: 1,
                name: "微信".into(),
            },
            category: Category {
                id: 1,
                parent_id: None,
                name: "餐饮".into(),
            },
            created_at: Utc.with_ymd_and_hms(2026, 10, 5, 12, 30, 0).unwrap(),
            remark: Some("午饭".into()),
        }];

        let mut collection = BillMonthCollection::new(month, 100_000);
        collection.total_income = 800_000;
        collection.total_expense = 2_084;
        collection.expense_by_category.insert(1, 1234);
        collection.expense_by_book.insert(1, 1234);
        collection.income_by_book.insert(2, 800_000);
        collection.income_by_category.insert(3, 800_000);

        let mut book_names = HashMap::new();
        book_names.insert(1, "微信".to_string());
        book_names.insert(2, "支付宝".to_string());
        let mut category_names = HashMap::new();
        category_names.insert(1, "餐饮".to_string());
        category_names.insert(3, "工资".to_string());

        app.report = Some(MonthReport {
            collection,
            book_names: book_names.clone(),
            category_names,
        });

        let mut year_collection = YearCollection::new(2026);
        year_collection.total_income = 9_600_000;
        year_collection.total_expense = 24_000;
        year_collection.income_by_book.insert(2, 9_600_000);
        year_collection.expense_by_book.insert(1, 24_000);
        year_collection.income_by_month[9] = 800_000;
        year_collection.expense_by_month[9] = 2_084;
        app.year_report = Some(YearReport {
            collection: year_collection,
            book_names,
        });

        app
    }

    /// 用 TestBackend 无头渲染一屏，按行还原成文本。
    fn render_screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("测试终端");
        terminal
            .draw(|frame| render(frame, app))
            .expect("渲染不应失败");
        let buffer = terminal.backend().buffer();
        let area = buffer.area;

        (0..area.height)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;

                while x < area.width {
                    let symbol = buffer
                        .cell((x, y))
                        .map(|cell| cell.symbol())
                        .unwrap_or(" ");
                    line.push_str(symbol);

                    // 宽字符（汉字）占两列，后一列是占位符，必须整格跳过；
                    // 否则还原出来的文本会在每个汉字后面多一个空格。
                    let advance = crate::common::table::display_width(symbol).max(1) as u16;
                    x += advance;
                }

                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tabs_show_every_page() {
        let app = sample_app();
        let screen = render_screen(&app, 110, 24);
        for page in Page::ALL {
            assert!(screen.contains(page.title()), "页签应包含「{}」：\n{screen}", page.title());
        }
    }

    #[test]
    fn overview_page_shows_summary_and_breakdown() {
        let app = sample_app();
        let screen = render_screen(&app, 100, 24);
        assert!(screen.contains("本月概览"), "{screen}");
        assert!(screen.contains("起始金额"), "{screen}");
        assert!(screen.contains("8000.00"), "应显示收入金额：\n{screen}");
        assert!(screen.contains("20.84"), "应显示支出金额：\n{screen}");
        assert!(screen.contains("7979.16"), "应显示本月净额：\n{screen}");
        assert!(screen.contains("支出构成"), "{screen}");
    }

    #[test]
    fn bills_page_lists_rows() {
        let mut app = sample_app();
        app.page = Page::Bills;
        let screen = render_screen(&app, 120, 24);
        assert!(screen.contains("2026-10-05"), "时间应只展示到天：\n{screen}");
        assert!(
            !screen.contains("2026-10-05 12:30:00"),
            "账单列表里不应出现具体时刻：\n{screen}"
        );
        assert!(screen.contains("12.34"), "{screen}");
        assert!(screen.contains("微信"), "{screen}");
        assert!(screen.contains("午饭"), "{screen}");
    }

    #[test]
    fn books_and_categories_pages_render() {
        let mut app = sample_app();

        app.page = Page::Books;
        let screen = render_screen(&app, 90, 20);
        assert!(screen.contains("账本"), "{screen}");
        assert!(screen.contains("支付宝"), "{screen}");

        app.page = Page::Categories;
        let screen = render_screen(&app, 90, 20);
        assert!(screen.contains("餐饮"), "{screen}");
        assert!(screen.contains("午餐"), "{screen}");
    }

    #[test]
    fn months_page_is_category_only_with_drilldown() {
        let mut app = sample_app();
        app.page = Page::Months;
        let screen = render_screen(&app, 120, 30);
        assert!(screen.contains("按分类 · 支出"), "{screen}");
        assert!(screen.contains("按分类 · 收入"), "{screen}");
        assert!(screen.contains("明细"), "{screen}");
        assert!(
            !screen.contains("按账本"),
            "月报不再分账本，账本口径归年报：\n{screen}"
        );
    }

    #[test]
    fn years_page_groups_by_book() {
        let mut app = sample_app();
        app.page = Page::Years;
        let screen = render_screen(&app, 120, 30);
        assert!(screen.contains("年报"), "{screen}");
        assert!(screen.contains("按账本 · 收入"), "{screen}");
        assert!(screen.contains("按账本 · 支出"), "{screen}");
        assert!(screen.contains("逐月走势"), "{screen}");
    }

    #[test]
    fn form_modal_renders_fields() {
        let mut app = sample_app();
        app.modal = Some(Modal::Form(Form::new_bill(
            vec!["微信".to_string(), "支付宝".to_string()],
            vec!["餐饮".to_string(), "  午餐".to_string()],
            None,
        )));
        let screen = render_screen(&app, 100, 30);
        assert!(screen.contains("新增账单"), "{screen}");
        assert!(screen.contains("金额"), "{screen}");
        assert!(screen.contains("账本"), "{screen}");
        assert!(screen.contains("分类"), "{screen}");
        assert!(screen.contains("Enter 提交"), "{screen}");
    }

    #[test]
    fn confirm_modal_renders_question() {
        let mut app = sample_app();
        app.modal = Some(Modal::Confirm {
            title: "确认删除".to_string(),
            message: "确定要删除账单 #7 吗？".to_string(),
            action: Action::DeleteBill(7),
        });
        let screen = render_screen(&app, 100, 30);
        assert!(screen.contains("确认删除"), "{screen}");
        assert!(screen.contains("账单 #7"), "{screen}");
        assert!(screen.contains("Esc / n 取消"), "{screen}");
    }

    /// 人工核对用：`cargo test -- --nocapture print_page_snapshots`
    #[test]
    fn print_page_snapshots() {
        let mut app = sample_app();
        for page in Page::ALL {
            app.page = page;
            let rendered = render_screen(&app, 100, 22);
            assert!(!rendered.trim().is_empty());
            println!("\n===== {} =====\n{rendered}", page.title());
        }

        app.modal = Some(Modal::Form(Form::new_bill(
            vec!["微信".to_string(), "支付宝".to_string()],
            vec!["餐饮".to_string(), "  午餐".to_string()],
            None,
        )));
        let rendered = render_screen(&app, 100, 22);
        assert!(rendered.contains("新增账单"));
        println!("\n===== 新增账单弹窗 =====\n{rendered}");
    }
}
