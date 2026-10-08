//! ratatui 渲染层：把 [`App`] 的状态画成一屏页面。
//!
//! 布局自上而下：页签栏 / 页面内容 / 状态栏 + 快捷键，弹窗叠加在最上层。

use std::collections::HashMap;

use chrono::Datelike;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};

use crate::common::db;
use crate::common::types::{AmountType, BookId, CategoryId};
use crate::common::{money, time};
use crate::plan::model::{DayCell, DayStatus, PlanMode, PlanProgress};
use crate::tui::app::{App, Form, Modal, Page, SettingsSection};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

/// 条形图宽度上限／下限；实际宽度按可用空间自适应。
const MAX_BAR_WIDTH: usize = 24;
const MIN_BAR_WIDTH: usize = 4;
/// 满格与空格用的方块字符（等宽字体都能显示）。
const BAR_FULL: &str = "█";
const BAR_EMPTY: &str = "░";

/// 日历一格里的空白（6 列，与 `day_span` 的宽度一致）。
const CALENDAR_BLANK: &str = "      ";

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
        Page::Months => render_months(frame, area, app),
        Page::Years => render_years(frame, area, app),
        Page::Settings => render_settings(frame, area, app),
    }
}

/// 设置页：上面「连接 | 账本」，中间「分类」，下面操作提示。
///
/// `←` `→` 在三个分区之间切焦点，当前分区的边框会高亮 —— `a` / `e` / `d` 只作用于它。
fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(5),
        Constraint::Length(5),
    ])
    .split(area);

    let top =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(rows[0]);

    let focus = app.settings_section;
    render_connection_panel(frame, top[0], app, focus == SettingsSection::Connection);
    render_books_panel(frame, top[1], app, focus == SettingsSection::Books);
    render_categories_panel(frame, rows[1], app, focus == SettingsSection::Categories);
    render_settings_tips(frame, rows[2], app);
}

/// 面板边框：当前分区用强调色，其余压暗。
fn panel_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    }
}

/// 分区标题，焦点所在的那块带个 ▶ 记号。
fn panel_title(section: SettingsSection, focused: bool) -> String {
    if focused {
        format!(" ▶ {} ", section.title())
    } else {
        format!(" {} ", section.title())
    }
}

fn render_connection_panel(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let settings = &app.connection;
    let lines = vec![
        kv_text("主机", &settings.host),
        kv_text("端口", &settings.port.to_string()),
        kv_text("用户名", &settings.user),
        kv_text("密码", &settings.masked_password()),
        kv_text("数据库", &settings.database),
        Line::from(Span::styled(
            format!("  {}", db::redact(&app.connection_url)),
            Style::default().fg(MUTED),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(panel_title(SettingsSection::Connection, focused))
                .border_style(panel_style(focused)),
        ),
        area,
    );
}

/// 操作提示随焦点分区变化，只说当前用得上的键。
fn render_settings_tips(frame: &mut Frame, area: Rect, app: &App) {
    let section = app.settings_section;
    let section_hint = match section {
        SettingsSection::Connection => "e 改连接并应用（保存后立即尝试切换）",
        SettingsSection::Books => "a 新增账本    e 改名    d 删除（名下还有账单会被拒绝）",
        SettingsSection::Categories => {
            "a 新增分类    e 改名 / 换父分类    d 删除（子树里有账单会被拒绝）"
        }
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "  ← →",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   切换分区（当前："),
            Span::styled(
                section.title().to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("）"),
        ]),
        Line::from(Span::styled(
            format!("      {section_hint}"),
            Style::default().fg(Color::White),
        )),
        Line::from(vec![
            Span::styled(
                "  i",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" 初始化    "),
            Span::styled(
                "t",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" 数据迁移    连接设置写进 "),
            Span::styled(".env", Style::default().fg(MUTED)),
            Span::raw("（含明文密码，已在 .gitignore）"),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" 操作 ")
                .border_style(Style::default().fg(MUTED)),
        ),
        area,
    );
}

/// 左对齐的「标签 + 值」行（设置页只用文本，不做金额格式化）。
fn kv_text(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<8}"), Style::default().fg(Color::Gray)),
        Span::styled(value.to_string(), Style::default().fg(Color::White)),
    ])
}

fn render_overview(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.report else {
        frame.render_widget(Paragraph::new("加载中…"), area);
        return;
    };

    // 概览的落脚点是「某一天」：上面一块按日期查看，下面是整月日历
    let rows = Layout::vertical([Constraint::Length(5), Constraint::Min(11)]).split(area);
    render_day_inspector(frame, rows[0], app);

    let bottom =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[1]);

    // 左：支出日历（绿/黄/红）
    render_calendar(frame, bottom[0], app);

    // 右：本月数字（整月构成归月报，这里不重复）
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
        bottom[1],
    );
}

/// 概览最上面那块：**按日期查看**当天的额度、开销、结余，以及本月计划。
///
/// `↑` `↓` 在当月内移动这个日期，日历上对应那格会带下划线。
fn render_day_inspector(frame: &mut Frame, area: Rect, app: &App) {
    let day = app.inspected_day();
    let date = app.month.with_day(day).unwrap_or(app.month);

    let title = if date == App::today() {
        format!(" {date} · 今天 · ↑ ↓ 换日期 ")
    } else {
        format!(" {date} · ↑ ↓ 换日期 · 按 m 回今天 ")
    };

    let block = Block::bordered()
        .title(Line::from(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .border_style(Style::default().fg(ACCENT));

    let Some(progress) = app.progress.as_ref() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  计划加载中…",
                Style::default().fg(MUTED),
            )))
            .block(block),
            area,
        );
        return;
    };

    let spent = progress.spent_on(day);
    let left_of_day = progress.daily_allowance - spent;
    let cumulative = progress.cumulative_through(day);
    // 额度余额：逐日滚下来的「还剩多少额度花」，花超前了是负数
    let allowance_left = progress.allowance_balance_through(day);

    let lines = vec![
        stat_row([
            ("当日开销", spent, Color::Red),
            ("当日额度", progress.daily_allowance, Color::White),
            (
                "当日结余",
                left_of_day,
                if left_of_day < 0 {
                    Color::LightYellow
                } else {
                    Color::Green
                },
            ),
        ]),
        stat_row([
            ("累计开销", cumulative, Color::Red),
            (
                "累计额度",
                allowance_left,
                if allowance_left < 0 {
                    Color::LightYellow
                } else {
                    Color::White
                },
            ),
            (
                "账上余额",
                app.balance_through_inspected_day().unwrap_or(0),
                ACCENT,
            ),
        ]),
        plan_line(progress),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// 一行放三项「标签 + 右对齐金额」，三列宽度固定，数字才是对齐的。
fn stat_row(stats: [(&str, AmountType, Color); 3]) -> Line<'static> {
    let mut spans = Vec::new();

    for (index, stat) in stats.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(stat_spans(stat));
    }

    Line::from(spans)
}

fn stat_spans((label, amount, color): (&str, AmountType, Color)) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{label:<6}"), Style::default().fg(MUTED)),
        Span::styled(
            format!("{:>10} 元", money::format_amount(amount)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]
}

/// 「本月计划」那一行；没设置过就提示去设置。
fn plan_line(progress: &PlanProgress) -> Line<'static> {
    if !progress.plan.is_set() {
        return Line::from(Span::styled(
            "  还没有开销计划 —— 按 e 一起设置起始金额与计划",
            Style::default().fg(MUTED),
        ));
    }

    // 整月已经超了上限 → 整行标红，这是概览最该一眼看到的事
    let style = if progress.over_limit() {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    Line::from(vec![
        Span::styled("  本月计划  ", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "上限 {} · 已用 {} · 剩余 {}",
                money::format_amount(progress.limit),
                money::format_amount(progress.spent),
                money::format_amount(progress.remaining())
            ),
            style,
        ),
        Span::styled("      按 e 修改", Style::default().fg(MUTED)),
    ])
}

/// 支出日历：一天一格，按 [`DayStatus`] 上色。
fn render_calendar(frame: &mut Frame, area: Rect, app: &App) {
    let Some(progress) = app.progress.as_ref() else {
        // 还没 reload 完时别留一块空白，给个占位
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled("  日历准备中…", Style::default().fg(MUTED))),
            ])
            .block(Block::bordered().title(" 支出日历 ")),
            area,
        );
        return;
    };

    // 表头列宽必须和 day_span 的 6 列一致，所以按同样规则拼，别手打空格
    let weekday_header: String = ["一", "二", "三", "四", "五", "六", "日"]
        .iter()
        .map(|name| format!("  {name}  "))
        .collect();

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        weekday_header,
        Style::default().fg(MUTED),
    ))];

    // 周一起排，月初前面留空格对齐
    let leading = progress.month.weekday().num_days_from_monday() as usize;
    let mut week: Vec<Span> = (0..leading).map(|_| Span::raw(CALENDAR_BLANK)).collect();

    for cell in &progress.cells {
        week.push(day_span(cell, cell.day == app.inspected_day()));
        if week.len() == 7 {
            lines.push(Line::from(std::mem::take(&mut week)));
        }
    }

    if !week.is_empty() {
        while week.len() < 7 {
            week.push(Span::raw(CALENDAR_BLANK));
        }
        lines.push(Line::from(week));
    }

    lines.push(Line::from(""));
    let over_hint = match progress.plan.mode {
        PlanMode::Cumulative => "累计超上限   ",
        PlanMode::DailyOnly => "单日超上限   ",
    };
    lines.push(Line::from(vec![
        Span::styled(
            "  绿",
            Style::default().bg(Color::Green).fg(Color::Black),
        ),
        Span::styled("≤日额度   ", Style::default().fg(MUTED)),
        Span::styled(
            "黄",
            Style::default().bg(Color::LightYellow).fg(Color::Black),
        ),
        Span::styled(">日额度   ", Style::default().fg(MUTED)),
        Span::styled("红", Style::default().bg(Color::Red).fg(Color::White)),
        Span::styled(over_hint, Style::default().fg(MUTED)),
        Span::styled("灰", Style::default().fg(MUTED)),
        Span::styled("无支出   ", Style::default().fg(MUTED)),
        Span::styled("下划线", Style::default().add_modifier(Modifier::UNDERLINED)),
        Span::styled("=正在查看", Style::default().fg(MUTED)),
    ]));

    let title = if progress.plan.is_set() {
        format!(
            " {} 支出日历 · 上限 {} · {} ",
            time::format_month(progress.month),
            money::format_amount(progress.limit),
            progress.plan.mode.label()
        )
    } else {
        format!(
            " {} 支出日历 · 按 e 设置计划 ",
            time::format_month(progress.month)
        )
    };

    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

/// 日历里的一格：固定 6 个字符，**只放天号**，按状态上色。
///
/// 用**底色**而不是字色：终端里黄字和绿字很容易看混，整块色块一眼就分得开。
/// `selected` 是概览页 `↑` `↓` 正在查看的那天，加下划线标出来。
fn day_span(cell: &DayCell, selected: bool) -> Span<'static> {
    let mut style = match cell.status {
        DayStatus::None => Style::default().fg(MUTED),
        DayStatus::WithinDaily => Style::default()
            .bg(Color::Green)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        DayStatus::OverDaily => Style::default()
            .bg(Color::LightYellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        DayStatus::OverLimit => Style::default()
            .bg(Color::Red)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    };

    if selected {
        style = style.add_modifier(Modifier::UNDERLINED);
    }

    Span::styled(format!("{:^6}", cell.day), style)
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

    // 中部：支出排行（分类 + 账本，可选） + 选中那一行的明细
    let middle = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)])
        .split(rows[1]);
    render_month_expense_pairs(frame, middle[0], app);
    render_month_selected_detail(frame, middle[1], app);

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

/// 月报的支出排行：可选中，右侧明细跟着走。
fn render_month_expense_pairs(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.selected_index(Page::Months);
    let (category_names, book_names) = app
        .report
        .as_ref()
        .map(|report| (report.category_names.clone(), report.book_names.clone()))
        .unwrap_or_default();

    render_expense_pairs(
        frame,
        area,
        "支出 · 分类 + 账本（↑↓ 选行看明细）",
        &app.month_expense_pairs(),
        &category_names,
        &book_names,
        Some(selected),
    );
}

/// 支出排行：**分类 + 账本** 成对比较（月报可选中，概览只读）。
///
/// 只按分类汇总会把「钱从哪本账走」抹平，房租这类集中在一本账上的大头就看不出来了。
fn render_expense_pairs(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[(CategoryId, BookId, AmountType)],
    category_names: &HashMap<u64, String>,
    book_names: &HashMap<u64, String>,
    selected: Option<usize>,
) {
    let total: i64 = entries.iter().map(|(_, _, amount)| *amount).sum();
    let bar_width = fit_bar_width(area.width, 8 + 6 + 12 + 7 + 4);
    // 概览只读（`None`）时不给位置提示
    let position = match selected {
        Some(selected) => position_hint(selected, entries.len()),
        None => String::new(),
    };

    let rows = entries
        .iter()
        .enumerate()
        .map(|(index, (category_id, book_id, amount))| {
            let (percent, bar) = share(*amount, total, bar_width);
            Row::new(vec![
                Cell::from(name_of(category_names, *category_id)),
                Cell::from(name_of(book_names, *book_id)),
                Cell::from(money::format_amount(*amount)),
                Cell::from(percent),
                Cell::from(Span::styled(bar, Style::default().fg(Color::Red))),
            ])
            .style(highlight(selected == Some(index)))
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Min(6),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(header(["分类", "账本", "支出", "占比", "构成"]))
    .column_spacing(1)
    .block(Block::bordered().title(Line::from(Span::styled(
        format!(" {title} · 合计 {}{position} ", money::format_amount(total)),
        Style::default().fg(Color::Red),
    ))));

    // 概览只读时 selected 传 None，取 0 只是为了不滚动
    render_following_table(frame, area, table, selected.unwrap_or(0));
}

fn render_month_selected_detail(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.month_selected_label() {
        Some(label) => format!(" 明细 · {label} "),
        None => " 明细 ".to_string(),
    };

    // 账本已经在标题里了，这里不再重复一列
    let rows = app
        .month_selected_bills()
        .iter()
        .map(|bill| {
            Row::new(vec![
                Cell::from(bill.id.to_string()),
                Cell::from(time::format_date(bill.created_at)),
                Cell::from(money::format_amount(bill.amount)),
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
            Constraint::Min(6),
        ],
    )
    .header(header(["ID", "日期", "金额", "备注"]))
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

    // 每月支出以「全年最高的那个月」为满格，一眼看出哪几个月花得多
    let peak = collection.expense_by_month.iter().copied().max().unwrap_or(0);
    let bar_width = fit_bar_width(rows[2].width, 8 + 14 * 3 + 4).min(12);

    let monthly = (0..12)
        .map(|index| {
            let income = collection.income_by_month[index];
            let expense = collection.expense_by_month[index];
            let (_, bar) = share(expense, peak, bar_width);
            Row::new(vec![
                Cell::from(format!("{} 月", index + 1)),
                Cell::from(money::format_amount(income)),
                Cell::from(money::format_amount(expense)),
                Cell::from(money::format_amount(income - expense)),
                Cell::from(Span::styled(bar, Style::default().fg(Color::Red))),
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
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(header(["月份", "收入", "支出", "净额", "支出分布"]))
    .column_spacing(1)
    .block(Block::bordered().title(" 逐月走势（支出条以全年最高月为满格） "));

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
                Cell::from(if bill.excluded { "×" } else { "✓" }),
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
            Constraint::Length(4),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(8),
        ],
    )
    .header(header([
        "ID", "日期", "方向", "计", "金额", "账本", "分类", "备注",
    ]))
    .column_spacing(1)
    .block(Block::bordered().title(format!(
        " {} 账单 · 共 {} 笔{} ",
        time::format_month(app.month),
        app.bills.len(),
        position_hint(selected, app.bills.len())
    )));

    render_following_table(frame, area, table, selected);
}

fn render_books_panel(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let selected = app.settings_index();
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
        .block(
            Block::bordered()
                .title(format!(
                    "{}· 共 {} 个{} ",
                    panel_title(SettingsSection::Books, focused),
                    app.books.len(),
                    position_hint(selected, app.books.len())
                ))
                .border_style(panel_style(focused)),
        );

    render_following_table(frame, area, table, selected);
}

fn render_categories_panel(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let names: HashMap<u64, String> = app
        .categories
        .iter()
        .map(|node| (node.category.id, node.category.name.clone()))
        .collect();
    let selected = app.settings_index();

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
    .block(
        Block::bordered()
            .title(format!(
                "{}· 共 {} 个{} ",
                panel_title(SettingsSection::Categories, focused),
                app.categories.len(),
                position_hint(selected, app.categories.len())
            ))
            .border_style(panel_style(focused)),
    );

    render_following_table(frame, area, table, selected);
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
        None if app.page == Page::Settings => {
            "Tab/1-5 切页    ←→ 切分区（连接/账本/分类）    a 新增    e 编辑    d 删除    i 初始化    t 数据迁移    q 退出"
        }
        None => {
            "Tab/←→/1-5 切页  ↑↓ 选择  [ ] 换月/年  a 新增  e 编辑/本月设置  d 删除  u 撤销  x 不计入  m 本月  r 刷新  q 退出"
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

    let total: i64 = entries.iter().map(|(_, amount)| *amount).sum();
    // 名称(至少 8) + 金额(12) + 占比(7) + 三段间距
    let bar_width = fit_bar_width(area.width, 8 + 12 + 7 + 3);

    let rows = entries
        .iter()
        .map(|(id, amount)| {
            let (percent, bar) = share(*amount, total, bar_width);
            Row::new(vec![
                Cell::from(name_of(names, *id)),
                Cell::from(money::format_amount(*amount)),
                Cell::from(percent),
                Cell::from(Span::styled(bar, Style::default().fg(color))),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(header(["名称", "金额", "占比", "构成"]))
    .column_spacing(1)
    .block(Block::bordered().title(Line::from(Span::styled(
        format!(" {title}（合计 {}） ", money::format_amount(total)),
        Style::default().fg(color),
    ))));

    frame.render_widget(table, area);
}

/// 计算一项占合计的比例，返回（百分比文本，定宽条形图）。
fn share(amount: i64, total: i64, width: usize) -> (String, String) {
    let width = width.max(1);

    if total <= 0 || amount <= 0 {
        return ("0.0%".to_string(), BAR_EMPTY.repeat(width));
    }

    let ratio = (amount as f64 / total as f64).clamp(0.0, 1.0);
    // 非零项至少给一格，否则占比很小的分类看起来像没有
    let filled = ((ratio * width as f64).round() as usize).clamp(1, width);

    (
        format!("{:.1}%", ratio * 100.0),
        format!(
            "{}{}",
            BAR_FULL.repeat(filled),
            BAR_EMPTY.repeat(width - filled)
        ),
    )
}

/// 从 id→名称 的表里取名字；记录被删过时给出可读的占位。
fn name_of(names: &HashMap<u64, String>, id: u64) -> String {
    names
        .get(&id)
        .cloned()
        .unwrap_or_else(|| format!("(已删除 id={id})"))
}

/// 条形图宽度按可用宽度自适应，窄终端也不会把表格挤爆。
///
/// `fixed_columns` 是同一行里其它列与间距占掉的字符数（不含两侧竖线）。
fn fit_bar_width(area_width: u16, fixed_columns: usize) -> usize {
    (area_width as usize)
        .saturating_sub(fixed_columns.saturating_add(2))
        .clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)
}

fn header<const N: usize>(titles: [&'static str; N]) -> Row<'static> {
    Row::new(titles)
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
}

/// 带状态渲染表格，让选中行始终滚动到可见区域。
///
/// 不用 `TableState` 时 ratatui 永远从第 0 行开始画：账单一旦超过一屏，
/// `↓` / `PageDown` 会把高亮行推出可见范围，界面看起来就是"选中项消失了"。
fn render_following_table(frame: &mut Frame, area: Rect, table: Table<'_>, selected: usize) {
    let mut state = TableState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(table, area, &mut state);
}

/// 列表标题里的位置提示，例如 ` · 第 12/340`；空列表时为空。
fn position_hint(selected: usize, total: usize) -> String {
    if total == 0 {
        String::new()
    } else {
        format!(" · 第 {}/{}", selected + 1, total)
    }
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
    use crate::plan::model::{PlanProgress, SpendingPlan};
    use crate::tui::app::{Action, Form, Modal};
    use chrono::{NaiveDate, TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_app() -> App {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut app = App::new(
            month,
            "postgres://postgres:postgres@127.0.0.1:5432/biller",
        );

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
            excluded: false,
        }];

        let mut collection = BillMonthCollection::new(month, 100_000);
        collection.total_income = 800_000;
        collection.total_expense = 2_084;
        collection.expense_by_category.insert(1, 1234);
        collection.expense_by_book.insert(1, 1234);
        collection.expense_by_category_book.insert((1, 1), 1234);
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

        // 概览页的日历需要计划进度：上限 4000.00，5 号花了 12.34
        let plan = SpendingPlan::new(12_000_00, 8_000_00);
        let mut spending = HashMap::new();
        spending.insert(5, 1234);
        app.plan = plan;
        app.progress = Some(PlanProgress::build(month, plan, None, &spending));

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
    fn overview_page_focuses_on_today() {
        let app = sample_app();
        let screen = render_screen(&app, 100, 26);

        // 本月数字仍在这里
        assert!(screen.contains("本月概览"), "{screen}");
        assert!(screen.contains("起始金额"), "{screen}");
        assert!(screen.contains("8,000.00"), "应显示收入金额：\n{screen}");
        assert!(screen.contains("20.84"), "应显示支出金额：\n{screen}");
        assert!(screen.contains("7,979.16"), "应显示本月净额：\n{screen}");

        // 整月构成归月报，概览放的是日历
        assert!(
            !screen.contains("支出构成"),
            "概览不该再放一份月报的整月构成：\n{screen}"
        );
        assert!(screen.contains("支出日历"), "概览应有支出日历：\n{screen}");
        assert!(screen.contains("上限 4,000.00"), "日历标题应带上限：\n{screen}");
        assert!(screen.contains("≤日额度"), "日历应有图例：\n{screen}");

        // 顶部是按日期查看的那块
        assert!(
            screen.contains("↑ ↓ 换日期"),
            "概览顶部应按日期查看：\n{screen}"
        );
        assert!(screen.contains("当日额度"), "{screen}");
        assert!(screen.contains("本月计划"), "{screen}");
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

    /// 造一批账单，用来验证长列表的滚动行为。
    fn many_bills(count: u64) -> Vec<BillEntity> {
        (1..=count)
            .map(|id| BillEntity {
                id,
                kind: BillKind::Expense,
                amount: 100 * id as i64,
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
                remark: Some(format!("第{id}笔")),
                excluded: false,
            })
            .collect()
    }

    #[test]
    fn long_bill_list_scrolls_selection_into_view() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.bills = many_bills(60);
        app.selected[Page::Bills.index()] = 55;

        // 一屏装不下 60 笔：选中第 56 笔时，它必须出现在画面上
        let screen = render_screen(&app, 100, 20);
        assert!(
            screen.contains("第56笔"),
            "选中行必须滚进可见区域，否则列表一长就是盲操作：\n{screen}"
        );
        assert!(
            !screen.contains("第1笔"),
            "屏幕装不下 60 行，首行不该同时可见：\n{screen}"
        );
        assert!(screen.contains("第 56/60"), "标题应给出位置：\n{screen}");
    }

    #[test]
    fn settings_page_hosts_books_and_categories() {
        let mut app = sample_app();
        app.page = Page::Settings;

        // 账本分区
        app.settings_section = SettingsSection::Books;
        let screen = render_screen(&app, 110, 26);
        assert!(screen.contains("▶ 账本"), "当前分区应有记号：\n{screen}");
        assert!(screen.contains("支付宝"), "{screen}");

        // 分类分区（树状缩进）
        app.settings_section = SettingsSection::Categories;
        let screen = render_screen(&app, 110, 26);
        assert!(screen.contains("▶ 分类"), "焦点应移到分类分区：\n{screen}");
        assert!(screen.contains("餐饮"), "{screen}");
        assert!(screen.contains("午餐"), "{screen}");

        // 换分区不该改变页面
        assert_eq!(app.page, Page::Settings);
    }

    #[test]
    fn months_page_compares_category_with_book() {
        let mut app = sample_app();
        app.page = Page::Months;
        let screen = render_screen(&app, 120, 30);

        assert!(screen.contains("支出 · 分类 + 账本"), "{screen}");
        assert!(screen.contains("按分类 · 收入"), "收入仍按分类汇总：\n{screen}");
        // 同一行里既有分类又有账本，大头藏不住
        assert!(screen.contains("餐饮"), "{screen}");
        assert!(screen.contains("微信"), "{screen}");
        assert!(
            screen.contains("明细 · 餐饮 @ 微信"),
            "明细要跟着选中那一行走：\n{screen}"
        );
        assert!(screen.contains("占比"), "{screen}");
        assert!(screen.contains("100.0%"), "{screen}");
        assert!(screen.contains('█'), "占比应配条形图：\n{screen}");
        assert!(
            !screen.contains("按账本"),
            "月报不做按账本的独立汇总，账本口径归年报：\n{screen}"
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
        assert!(screen.contains("占比"), "按账本支出应给出占比：\n{screen}");
        assert!(screen.contains("100.0%"), "{screen}");
        assert!(screen.contains('█'), "占比应配条形图：\n{screen}");
        assert!(
            screen.contains("支出分布"),
            "逐月走势应带支出分布条：\n{screen}"
        );
    }

    #[test]
    fn settings_page_renders_connection_and_hides_password() {
        let mut app = sample_app();
        app.page = Page::Settings;
        let screen = render_screen(&app, 110, 28);

        assert!(screen.contains("数据库连接"), "{screen}");
        assert!(screen.contains("127.0.0.1"), "{screen}");
        assert!(screen.contains("5432"), "{screen}");
        assert!(screen.contains("biller"), "{screen}");
        assert!(screen.contains("初始化"), "{screen}");
        assert!(screen.contains("数据迁移"), "{screen}");
        assert!(screen.contains("***"), "密码应打码显示：\n{screen}");
        assert!(
            !screen.contains(":postgres@"),
            "不应把明文密码拼进连接串：\n{screen}"
        );
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
