//! ratatui 渲染层：把 [`App`] 的状态画成一屏页面。
//!
//! 视觉规范（改任何页面前先看这段，别在页面里另立门户）：
//! - 配色只用 [`ACCENT`] / [`GOOD`] / [`BAD`] / [`WARN`] / [`TEXT`] / [`MUTED`] / [`BORDER`]
//!   七个语义色（Tokyo Night 系），不铺全局背景；
//! - 所有面板圆角边框（`╭╮╰╯`），标题嵌在顶部边框上（`╭─ 标题 ───╮`），左右各 1 列内边距；
//! - 焦点面板边框 [`ACCENT`] 且标题加粗；非焦点面板边框 [`BORDER`]、标题 [`MUTED`]；
//! - 三级文字：关键数字（TEXT / 语义色 + 加粗）> 常规（TEXT）> 说明与表头（MUTED）；
//! - 金额统一走 `money::format_amount`（千分位），数字列右对齐；长文本截断补 `…`；
//! - 布局自上而下：1 行顶栏 / 页面内容 / 2 行底栏（状态 + 上下文快捷键），弹窗叠加在最上层。

use std::collections::HashMap;

use chrono::Datelike;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap,
};

use crate::bill::model::{BillEntity, BillKind};
use crate::common::db;
use crate::common::table::{display_width, pad_right};
use crate::common::types::AmountType;
use crate::common::{money, time};
use crate::month::service::MonthReport;
use crate::plan::model::{DayStatus, total_limit};
use crate::tui::app::{App, Form, Modal, Page, ReportTab, SettingsFocus, SettingsSection};

// ---------------------------------------------------------------------------
// 语义色（唯一的取色入口；页面里不要再出现别的 Color 字面量）
// ---------------------------------------------------------------------------

/// 焦点 / 键名 / 主色。
const ACCENT: Color = Color::Rgb(125, 207, 255);
/// 收入 / 正常 / 未超额度。
const GOOD: Color = Color::Rgb(158, 206, 106);
/// 支出 / 超限 / 错误。
const BAD: Color = Color::Rgb(247, 118, 142);
/// 待处理 / 提醒。
const WARN: Color = Color::Rgb(224, 175, 104);
/// 正文：跟随终端默认前景，绝不覆盖全局背景。
const TEXT: Color = Color::Reset;
/// 表头 / 说明 / 未选中。
const MUTED: Color = Color::Rgb(86, 95, 137);
/// 非焦点面板边框。
const BORDER: Color = Color::Rgb(59, 66, 97);

/// 面板左右内边距（列）。
const PAD: u16 = 1;

/// 条形图宽度上下限；实际宽度按可用空间自适应。
const MAX_BAR_WIDTH: usize = 24;
const MIN_BAR_WIDTH: usize = 4;
/// 满格与空格用的方块字符（等宽字体都能显示）。
const BAR_FULL: &str = "█";
const BAR_EMPTY: &str = "░";

/// 日历一格的空白（6 列，与 `day_span` 的宽度一致）。

/// 日历一格占的列数（默认值）；面板放不下时会自动收窄。
const CALENDAR_CELL_WIDTH: usize = 8;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        // 顶栏只有 1 行：品牌 + 月份 + 胶囊页签 + 连接状态
        Constraint::Length(1),
        Constraint::Min(3),
        // 底栏固定 2 行：状态 1 行 + 上下文快捷键 1 行
        Constraint::Length(2),
    ])
    .split(frame.area());

    render_topbar(frame, chunks[0], app);
    render_body(frame, chunks[1], app);
    render_footer(frame, chunks[2], app);

    if app.modal.is_some() {
        render_modal(frame, app);
    }
}

// ---------------------------------------------------------------------------
// 顶栏
// ---------------------------------------------------------------------------

/// 1 行顶栏：左「品牌 + 当前月 + 换月键 + 设置入口」、中「胶囊页签」、右「连接状态」。
fn render_topbar(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;

    // 左：biller 2026-10  [ / ] 换月  , 设置
    //
    // 设置不在页签队列里（一年点不了几次），但入口必须一直看得见——
    // 只在帮助里写 `,` 等于没有入口。在设置页时提示改成「, 返回」。
    let settings_hint = if app.page == Page::Settings { ", 返回" } else { ", 设置" };
    let left = Line::from(vec![
        Span::styled("biller", bold(ACCENT)),
        Span::raw(" "),
        Span::styled(time::format_month(app.month), bold(WARN)),
        Span::raw("  "),
        Span::styled("[ / ] 换月", muted()),
        Span::raw("  "),
        Span::styled(settings_hint, muted()),
    ]);
    let left_w = left.width();

    // 页签宽度跟着页数走，别写死：标题都是 2 个字，带序号的胶囊 8 列、不带序号 6 列，组间 1 列
    let tabs = Page::ALL.len();
    let gaps = tabs.saturating_sub(1);
    let numeric_w = tabs * 8 + gaps;
    let compact_w = tabs * 6 + gaps;
    // 窄终端先砍掉右槽，再砍掉页签序号，保证左边永远先画出来
    let (tab_w, numeric, right_w) = if width >= left_w + 1 + numeric_w + 16 {
        (numeric_w, true, width - left_w - 1 - numeric_w)
    } else if width >= left_w + 1 + compact_w {
        (compact_w, false, 0)
    } else {
        // 实在窄：左边都放不下时优先留左侧，页签和右槽都不要
        (0, false, 0)
    };

    let cols = Layout::horizontal([
        Constraint::Length(left_w as u16),
        Constraint::Length(tab_w as u16),
        Constraint::Length(right_w as u16),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(left), cols[0]);

    // 中：胶囊页签，当前页反色（ACCENT 底 + 黑字加粗）
    if tab_w > 0 {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, page) in Page::ALL.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            let label = if numeric {
                format!(" {} {} ", index + 1, page.title())
            } else {
                format!(" {} ", page.title())
            };
            let style = if *page == app.page {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            };
            spans.push(Span::styled(label, style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), cols[1]);
    }

    // 右：连接状态（右对齐）
    if right_w > 0 {
        let (text, color) = connection_status(app);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color))))
                .alignment(Alignment::Right),
            cols[2],
        );
    }
}

/// 右上角的连接状态：本地文件 / 远程 PostgreSQL / 还没连接。
fn connection_status(app: &App) -> (String, Color) {
    if app.needs_setup {
        return ("未连接 · 按 6 配置".to_string(), WARN);
    }

    if app.connection_url.starts_with("sqlite:") {
        let name = app
            .sqlite_path
            .rsplit(|character| character == '/' || character == '\\')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("biller.db");
        return (truncate(&format!("本地 · {name}"), 24), MUTED);
    }

    (
        truncate(&format!("PostgreSQL · {}", app.connection.host), 24),
        MUTED,
    )
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    // 还没连上库时先显示引导页；但设置页照常放行，否则用户没有配库的入口
    if app.needs_setup && app.page != Page::Settings {
        render_setup(frame, area, app);
        return;
    }

    match app.page {
        Page::Overview => render_overview(frame, area, app),
        Page::Plan => render_plan(frame, area, app),
        Page::Bills => render_bills(frame, area, app),
        Page::Reports => render_reports(frame, area, app),
        Page::Settings => render_settings(frame, area, app),
    }
}

// ---------------------------------------------------------------------------
// 面板骨架 / 文案样式
// ---------------------------------------------------------------------------

/// 面板边框：圆角 + 左右 1 列内边距；焦点面板用 ACCENT，其余用 BORDER。
fn frame_block(focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(PAD))
        .border_style(Style::default().fg(edge_color(focused)))
}

fn edge_color(focused: bool) -> Color {
    if focused { ACCENT } else { BORDER }
}

/// 面板标题文字：焦点面板 ACCENT 加粗，非焦点 MUTED。
fn title_span(text: impl Into<String>, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(text.into(), bold(ACCENT))
    } else {
        Span::styled(text.into(), muted())
    }
}

/// 带「标题嵌在边框上」的面板：`╭─ 标题 ─────╮`。
fn titled_block(focused: bool, title: Vec<Span<'static>>) -> Block<'static> {
    let edge = edge_color(focused);
    let mut spans = vec![Span::styled("─ ", Style::default().fg(edge))];
    spans.extend(title);
    spans.push(Span::raw(" "));

    frame_block(focused).title(Line::from(spans))
}

/// 单段标题的快捷写法。
fn block_with_title(focused: bool, text: impl Into<String>) -> Block<'static> {
    titled_block(focused, vec![title_span(text, focused)])
}

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn muted() -> Style {
    Style::default().fg(MUTED)
}

fn plain() -> Style {
    Style::default().fg(TEXT)
}

/// 按显示宽度左补空格（中文标签右对齐用）。
fn pad_left(text: &str, width: usize) -> String {
    let used = display_width(text);
    format!("{}{}", " ".repeat(width.saturating_sub(used)), text)
}

/// 超宽截断并补 `…`；不超宽就原样返回。
fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_string();
    }

    let limit = width.saturating_sub(1); // 给 … 留一格
    let mut out = String::new();
    let mut used = 0;

    for character in text.chars() {
        let character_width = display_width(&character.to_string());
        if used + character_width > limit {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// 占位 / 空状态
// ---------------------------------------------------------------------------

/// 数据还没读回来时的占位：给一句「在做什么」，而不是一个光秃秃的「加载中」。
fn render_loading(frame: &mut Frame, area: Rect, title: &str, message: &str) {
    render_placeholder(frame, area, block_with_title(false, title), message, "");
}

/// 空列表 / 加载中的占位：横竖都居中的提示（「现在是什么」+「下一步按什么」）。
fn render_placeholder(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    message: &str,
    hint: &str,
) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let body_height = if hint.is_empty() { 1 } else { 3 };
    let top = inner_height.saturating_sub(body_height) / 2;

    let mut lines: Vec<Line<'static>> = (0..top).map(|_| Line::from("")).collect();
    lines.push(Line::from(Span::styled(
        truncate(message, area.width.saturating_sub(2 + 2 * PAD) as usize),
        bold(TEXT),
    )));

    if !hint.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            truncate(hint, area.width.saturating_sub(2 + 2 * PAD) as usize),
            muted(),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(block),
        area,
    );
}

// ---------------------------------------------------------------------------
// 引导页
// ---------------------------------------------------------------------------

/// 连不上数据库时的引导页：告诉用户去哪儿配、配成什么样。
fn render_setup(frame: &mut Frame, area: Rect, app: &App) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("还没有可用的数据库连接", bold(WARN))),
        Line::from(Span::styled(
            "配好之后才能记账；数据都存在你自己的库里，换机器把库带走就行。",
            muted(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("当前目标  ", muted()),
            Span::styled(db::redact(&app.connection_url), plain()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "按 6 进入「设置 → 数据库连接」，把连接串改成下面任意一种：",
            plain(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  · 本地文件（零配置）  ", bold(ACCENT)),
            Span::styled("sqlite:C:/Users/你/.biller/biller.db", muted()),
        ]),
        Line::from(vec![
            Span::styled("  · 远程 PostgreSQL     ", bold(ACCENT)),
            Span::styled("postgres://user:pass@127.0.0.1:5432/biller", muted()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "改完回车保存，界面会立刻切过去；数据也都还在。",
            muted(),
        )),
    ];

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(PAD))
        .border_style(Style::default().fg(WARN))
        .title(Line::from(vec![
            Span::styled("─ ", Style::default().fg(WARN)),
            Span::styled("首次使用 · 配置数据库", bold(WARN)),
            Span::raw(" "),
        ]));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

// ---------------------------------------------------------------------------
// 报表页（月报 / 年报）
// ---------------------------------------------------------------------------

/// 报表页：月报与年报是同一个问题的两个时间尺度，共用一页，`m` / `y` 切。
fn render_reports(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(6)]).split(area);
    render_report_switcher(frame, rows[0], app);
    match app.report_tab {
        ReportTab::Month => render_months(frame, rows[1], app),
        ReportTab::Year => render_years(frame, rows[1], app),
    }
}

/// 一行胶囊：亮着的那个就是现在看的时间尺度。
fn render_report_switcher(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (tab, label)) in [(ReportTab::Month, "月报"), (ReportTab::Year, "年报")]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if tab == app.report_tab {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            muted()
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    spans.push(Span::styled("   m 月报 · y 年报", muted()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------------
// 设置页
// ---------------------------------------------------------------------------

/// 设置页：**左边一级菜单、右边二级内容**，底部固定「操作」面板。
fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    // 菜单列 20：最长的「运维 · 数据库」是双宽字，18 列会被截成「运维 · 数据」
    let cols = Layout::horizontal([Constraint::Length(20), Constraint::Min(30)]).split(area);

    render_settings_menu(frame, cols[0], app);

    let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(4)]).split(cols[1]);
    match app.settings_section {
        SettingsSection::Connection => render_connection_panel(frame, rows[0], app),
        SettingsSection::Categories => render_categories_panel(frame, rows[0], app),
    }
    render_settings_actions(frame, rows[1], app);
}

/// 一级菜单：分类 / 数据库连接。
///
/// 焦点在菜单上时选中项整行反色 + 前置 ▶；焦点进了右侧列表后不再反色，
/// 只留一个 ACCENT 的 ▶——这样一眼能看出「现在按 ↑↓ 移动的是哪一级」。
fn render_settings_menu(frame: &mut Frame, area: Rect, app: &App) {
    let inner = area.width.saturating_sub(2 + 2 * PAD) as usize;
    let menu_focused = app.settings_focus == SettingsFocus::Menu;
    let mut lines: Vec<Line<'static>> = Vec::new();

    for section in SettingsSection::ALL.iter() {
        let current = *section == app.settings_section;
        let marker = if current { "▶ " } else { "  " };
        let text = pad_right(&format!("{marker}{}", section.title()), inner);
        let style = if current && menu_focused {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if current {
            bold(ACCENT)
        } else {
            muted()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(
        Paragraph::new(lines).block(block_with_title(menu_focused, "设置")),
        area,
    );
}

/// 数据库连接分区：解析后的参数（kv 网格）+ 连接串 + 两种模式怎么选。
fn render_connection_panel(frame: &mut Frame, area: Rect, app: &App) {
    let settings = &app.connection;
    let local = app.connection_url.starts_with("sqlite:");
    let inner = area.width.saturating_sub(2 + 2 * PAD) as usize;
    // 每行两组 kv：两组各占 cell 列，中间再隔 2 列；cell 要留出这 2 列，否则会被 wrap 折行
    let cell = inner.saturating_sub(2) / 2;

    let pairs: Vec<(&'static str, String)> = if local {
        vec![
            ("模式", "本地文件（零配置）".to_string()),
            ("路径", app.sqlite_path.clone()),
        ]
    } else {
        vec![
            ("模式", "远程 PostgreSQL".to_string()),
            ("主机", settings.host.clone()),
            ("端口", settings.port.to_string()),
            ("用户名", settings.user.clone()),
            ("密码", settings.masked_password()),
            ("数据库", settings.database.clone()),
        ]
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for chunk in pairs.chunks(2) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, (label, value)) in chunk.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            let head = format!("{} ", pad_right(label, 6));
            let used = display_width(&head) + display_width(value);
            spans.push(Span::styled(head, muted()));
            spans.push(Span::styled(
                truncate(value, cell.saturating_sub(8)),
                plain(),
            ));
            spans.push(Span::raw(" ".repeat(cell.saturating_sub(used).max(1))));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("连接串 ", muted()),
        Span::styled(
            truncate(&db::redact(&app.connection_url), inner.saturating_sub(10)),
            plain(),
        ),
    ]));
    let intro_len = lines.len();

    // 面板够高才补「两种模式怎么选」；且贴底排：上面参数、中间留白、底部说明
    if area.height >= 15 {
        let tail = vec![
            Line::from(Span::styled("怎么选", muted())),
            Line::from(Span::styled("· 本地文件：零配置，单机自己用就够", muted())),
            Line::from(Span::styled(
                "· PostgreSQL：多人 / 多机共享同一个库",
                muted(),
            )),
        ];
        let inner_height = area.height.saturating_sub(2) as usize;
        let gap = inner_height.saturating_sub(intro_len + tail.len());
        for _ in 0..gap {
            lines.push(Line::from(""));
        }
        lines.extend(tail);
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block_with_title(true, "数据库连接")),
        area,
    );
}

/// 分类分区：树状缩进 + 父分类列。
fn render_categories_panel(frame: &mut Frame, area: Rect, app: &App) {
    let names: HashMap<u64, String> = app
        .categories
        .iter()
        .map(|node| (node.category.id, node.category.name.clone()))
        .collect();
    let selected = app.settings_index();
    let focused = app.settings_focus == SettingsFocus::Detail;
    let title = format!(
        "分类 · 共 {} 个{}",
        app.categories.len(),
        if focused {
            position_hint(selected, app.categories.len())
        } else {
            String::new()
        }
    );

    if app.categories.is_empty() {
        render_placeholder(
            frame,
            area,
            block_with_title(true, title),
            "还没有分类",
            "按 a 新增；分类可以多层，先用「餐饮」「交通」打底",
        );
        return;
    }

    let rows = app
        .categories
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let highlight_row = focused && index == selected;
            let indent = if node.depth == 0 {
                String::new()
            } else {
                format!("{}└─ ", "  ".repeat(node.depth - 1))
            };
            let parent = match node.category.parent_id {
                Some(id) => names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("id={id}")),
                None => "-".to_string(),
            };

            if highlight_row {
                Row::new(vec![
                    Cell::from(node.category.id.to_string()),
                    Cell::from(format!("{indent}{}", node.category.name)),
                    Cell::from(parent),
                ])
                .style(highlight(true))
            } else {
                Row::new(vec![
                    Cell::from(Span::styled(node.category.id.to_string(), muted())),
                    Cell::from(Span::styled(
                        format!("{indent}{}", node.category.name),
                        plain(),
                    )),
                    Cell::from(Span::styled(parent, muted())),
                ])
            }
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
    .header(
        Row::new(vec![
            header_cell("ID"),
            header_cell("分类"),
            header_cell("父分类"),
        ])
        .style(header_style()),
    )
    .column_spacing(1)
    .block(block_with_title(focused, title));

    render_following_table(frame, area, table, focused.then_some(selected));
}

/// 底部「操作」面板：键位随当前分区与焦点变化，只说现在用得上的。
fn render_settings_actions(frame: &mut Frame, area: Rect, app: &App) {
    let mut actions: Vec<(&'static str, &'static str)> = match app.settings_focus {
        SettingsFocus::Menu => vec![("↑ ↓", "选分区")],
        SettingsFocus::Detail => vec![("↑ ↓", "选条目"), ("Esc", "回菜单")],
    };

    let footnote: &'static str = match app.settings_section {
        SettingsSection::Connection => {
            actions.push(("e", "改连接并立即应用"));
            actions.push(("i", "初始化数据库"));
            actions.push(("t", "整库数据迁移"));
            "设置写进 config.toml / .env（含明文密码，已在 .gitignore）"
        }
        SettingsSection::Categories => {
            if app.settings_focus == SettingsFocus::Menu {
                actions.push(("Enter", "进列表"));
            } else {
                actions.push(("Enter", "改名 / 换父分类"));
            }
            actions.push(("a", "新增分类"));
            actions.push(("e", "改名 / 换父分类"));
            actions.push(("d", "删除"));
            "分类是树，子分类挂在父分类下面；改名不影响历史账单"
        }
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (key, hint)) in actions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled((*key).to_string(), bold(ACCENT)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled((*hint).to_string(), muted()));
    }

    let lines = vec![
        Line::from(spans),
        Line::from(Span::styled(footnote, muted())),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block_with_title(false, "操作")),
        area,
    );
}

// ---------------------------------------------------------------------------
// 概览
// ---------------------------------------------------------------------------

/// 概览：本月一块大字条 → 支出日历 | 今日 + 最近动账。
///
/// 一屏只回答一个问题：「这个月我还剩多少能花」。日 / 月的细节各自只出现一次，
/// 键位统一交给底栏和 `?`，页内不再重复。
fn render_overview(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(4), // 本月：已花 / 额度 / 还剩 + 用量条
        Constraint::Min(8),    // 日历 | 今日 + 最近动账
    ])
    .split(area);

    render_month_bar(frame, rows[0], app);

    let columns =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[1]);
    render_calendar(frame, columns[0], app);

    let right = Layout::vertical([Constraint::Length(3), Constraint::Min(6)]).split(columns[1]);
    render_today(frame, right[0], app);
    render_recent_bills(frame, right[1], app);
}

/// 本月条：已花 / 额度 / 还剩 → 用量条 + 日均。
///
/// 三个数字是同一个算式里的量：还剩 = 额度 − 已花。收入、结余这类「资产视角」
/// 的数字归月报，这里不掺，免得一个面板要读完六个数字才敢下判断。
fn render_month_bar(frame: &mut Frame, area: Rect, app: &App) {
    let block = block_with_title(false, "本月");
    let inner = block.inner(area);
    let width = inner.width as usize;

    let Some(report) = app.report.as_ref() else {
        render_placeholder(
            frame,
            area,
            block,
            "正在汇总本月账单",
            "收入与支出就位后这里会出现本月的用量",
        );
        return;
    };

    let progress = app.progress.as_ref();
    let spent = progress
        .map(|progress| progress.spent)
        .unwrap_or(report.collection.total_expense);
    let limit = progress
        .map(|progress| progress.limit)
        .unwrap_or_else(|| total_limit(&app.plans));
    let average = progress.map(|progress| progress.daily_average).unwrap_or(0);
    let days = month_shape(app.month.year(), app.month.month()).1;

    // 没分额度时不要假装有：给一个「还没设」，再指个去路
    if limit <= 0 {
        let lines = vec![
            stat_row(&[("已花", money::format_amount(spent), bold(BAD))], width),
            Line::from(Span::styled("还没分额度 · 按 2 去计划页分一下", muted())),
        ];
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let remaining = limit - spent;
    let remaining_style = bold(if remaining < 0 { BAD } else { GOOD });
    let usage_style = if spent > limit { BAD } else { ACCENT };
    let bar_width = (width / 4).clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH);
    let (usage, bar) = share(spent, limit, bar_width);
    let projected = money::format_amount(average * days as i64);

    let lines = if width >= 70 {
        vec![
            stat_row(
                &[
                    ("已花", money::format_amount(spent), bold(BAD)),
                    ("额度", money::format_amount(limit), plain()),
                    ("还剩", money::format_amount(remaining), remaining_style),
                ],
                width,
            ),
            Line::from(vec![
                Span::styled(bar, Style::default().fg(usage_style)),
                Span::raw(" "),
                Span::styled(usage, bold(usage_style)),
                Span::styled("  日均 ", muted()),
                Span::styled(money::format_amount(average), plain()),
                Span::styled(" · 这么花下去月底大约 ", muted()),
                Span::styled(projected, plain()),
            ]),
        ]
    } else {
        vec![
            stat_row(
                &[
                    ("已花", money::format_amount(spent), bold(BAD)),
                    ("还剩", money::format_amount(remaining), remaining_style),
                ],
                width,
            ),
            Line::from(vec![
                Span::styled(bar, Style::default().fg(usage_style)),
                Span::raw(" "),
                Span::styled(usage, bold(usage_style)),
                Span::styled("  额度 ", muted()),
                Span::styled(money::format_amount(limit), plain()),
            ]),
        ]
    };

    frame.render_widget(Paragraph::new(lines).block(block), area);
}


/// 最近动账：按「日期 + 星期」分组，每天一行小计 + 最多两条明细。
fn render_recent_bills(frame: &mut Frame, area: Rect, app: &App) {
    let block = block_with_title(false, "最近动账");
    let inner = block.inner(area);
    let width = inner.width as usize;

    if app.bills.is_empty() {
        render_placeholder(
            frame,
            area,
            block,
            "本月还没有账单",
            "按 a 记一笔 · [ ] 换个月",
        );
        return;
    }

    let capacity = inner.height as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (date, group) in bills_by_day(&app.bills) {
        if lines.len() + 1 > capacity {
            break;
        }
        lines.push(day_header_line(date, &group));
        for bill in group.iter().take(2) {
            if lines.len() + 1 > capacity {
                break;
            }
            lines.push(compact_bill_line(bill, width));
        }
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}


/// 今日：日期在标题上，正文只两个数字——今天花了多少、到今天的额度还剩多少。
///
/// 「今天还能花多少」是这里唯一的算术；累计量和账上余额都归本月条与月报。
fn render_today(frame: &mut Frame, area: Rect, app: &App) {
    let day = app.inspected_day();
    let progress = app.progress.as_ref();

    let day_spent = progress
        .and_then(|progress| progress.cells.iter().find(|cell| cell.day == day))
        .map(|cell| cell.spent)
        .unwrap_or(0);
    let day_allowance = progress
        .map(|progress| progress.allowance_through(day))
        .unwrap_or(0);
    let day_balance = progress
        .map(|progress| progress.allowance_balance_through(day))
        .unwrap_or(0);

    // 窄屏塞不下「年 + 星期 + 额度」，就去掉年份只留月日，别让标题被裁一半
    let narrow = area.width < 40;
    let title = if narrow {
        format!(
            "{:02}-{:02} {} · 额度 {}",
            app.month.month(),
            day,
            weekday_name(app.month, day),
            money::format_amount(day_allowance)
        )
    } else {
        format!(
            "{}-{:02}-{:02} {} · 今日额度 {}",
            app.month.year(),
            app.month.month(),
            day,
            weekday_name(app.month, day),
            money::format_amount(day_allowance)
        )
    };
    let block = block_with_title(true, title);
    let width = block.inner(area).width as usize;

    let day_color = if day_allowance > 0 && day_spent > day_allowance {
        BAD
    } else {
        TEXT
    };
    // 窄屏放不下六个字，标签缩成两个字；数字绝不能截断
    let (spent_label, left_label) = if narrow { ("已花", "还剩") } else { ("今日已花", "今日还剩") };
    let line = stat_row(
        &[
            (spent_label, money::format_amount(day_spent), bold(day_color)),
            (
                left_label,
                // 还剩多少就是多少钱，正数不必带 `+`——带上加号像在报涨幅
                money::format_amount(day_balance),
                bold(if day_balance < 0 { BAD } else { GOOD }),
            ),
        ],
        width,
    );

    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// 支出日历：7 列方格 + 底部两行图例 + 一行迷你进度条。
fn render_calendar(frame: &mut Frame, area: Rect, app: &App) {
    // 图例搬进边框标题：三种颜色各是什么一眼对上，不再单占一行
    let block = titled_block(
        false,
        vec![
            Span::styled("支出日历 · ", plain()),
            Span::styled("■", Style::default().fg(GOOD)),
            Span::styled(" 正常 ", muted()),
            Span::styled("■", Style::default().fg(WARN)),
            Span::styled(" 超日额度 ", muted()),
            Span::styled("■", Style::default().fg(BAD)),
            Span::styled(" 超本月 ", muted()),
        ],
    );
    let inner = block.inner(area);
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;

    let selected = app.inspected_day();
    let statuses: HashMap<u32, DayStatus> = app
        .progress
        .as_ref()
        .map(|progress| {
            progress
                .cells
                .iter()
                .map(|cell| (cell.day, cell.status))
                .collect()
        })
        .unwrap_or_default();

    let (leading, days) = month_shape(app.month.year(), app.month.month());
    // 每天固定等宽格：日号与当天金额右对齐到同一条列边界，逐日扫下来不会错行
    let cell = CALENDAR_CELL_WIDTH.min((inner_width / 7).max(3));
    let grid_width = 7 * cell;
    let left_pad = inner_width.saturating_sub(grid_width) / 2;
    let pad = " ".repeat(left_pad);
    let blank = " ".repeat(cell);

    let weeks = (leading as usize + days as usize).div_ceil(7);
    // 面板够高才用「日号 + 当天金额」两行一格；放不下就退回单行
    let two_rows = inner_height > weeks * 2;

    // 每天的实际开销：日期格下面那行顺手给出金额，日历不用再另排数据
    let spent_by_day: HashMap<u32, AmountType> = app
        .progress
        .as_ref()
        .map(|progress| {
            progress
                .cells
                .iter()
                .map(|day| (day.day, day.spent))
                .collect()
        })
        .unwrap_or_default();

    let weekday = ["一", "二", "三", "四", "五", "六", "日"];
    let header_spans: Vec<Span<'static>> = weekday
        .iter()
        .map(|label| Span::styled(pad_left(label, cell), muted()))
        .collect();
    let mut lines = vec![Line::from(
        std::iter::once(Span::raw(pad.clone()))
            .chain(header_spans)
            .collect::<Vec<_>>(),
    )];

    for row in 0..weeks {
        let mut day_line: Vec<Span<'static>> = vec![Span::raw(pad.clone())];
        let mut amount_line: Vec<Span<'static>> = vec![Span::raw(pad.clone())];
        for column in 0..7usize {
            let index = row * 7 + column;
            if index < leading as usize || index >= leading as usize + days as usize {
                day_line.push(Span::raw(blank.clone()));
                amount_line.push(Span::raw(blank.clone()));
                continue;
            }
            let day = (index - leading as usize + 1) as u32;
            let status = statuses.get(&day).copied().unwrap_or(DayStatus::None);
            day_line.push(calendar_cell(day, status, day == selected, cell));
            let spent = spent_by_day.get(&day).copied().unwrap_or(0);
            amount_line.push(if spent > 0 {
                Span::styled(
                    pad_left(&truncate(&money::format_amount(spent), cell), cell),
                    muted(),
                )
            } else {
                Span::raw(blank.clone())
            });
        }
        lines.push(Line::from(day_line));
        if two_rows {
            lines.push(Line::from(amount_line));
        }
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// 一格日历：日期按语义色上色；选中的那天反色加下划线（整格高亮）。
fn calendar_cell(day: u32, status: DayStatus, selected: bool, cell: usize) -> Span<'static> {
    let color = match status {
        DayStatus::None => MUTED,
        DayStatus::WithinDaily => GOOD,
        DayStatus::OverDaily => WARN,
        DayStatus::OverLimit => BAD,
    };
    let spent = !matches!(status, DayStatus::None);
    let text = pad_left(&format!("{day:>2}"), cell);

    let style = if selected {
        // 选中的那天整格反色：和表格选中行同一套视觉语言
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else if spent {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };

    Span::styled(text, style)
}

// ---------------------------------------------------------------------------
// 计划
// ---------------------------------------------------------------------------

/// 计划：左边是全部计划，右边给出「额度合计 vs 收入」的覆盖结论。
///
/// 计划 = 一个名字 + 可选上限：账单记在哪个计划名下，那个计划的「已花」就自动涨。
/// 有上限的判超支，没上限的只从 0 开始累计——两种都在这张表里，不再分两套概念。
fn render_plan(frame: &mut Frame, area: Rect, app: &App) {
    let count = app.plans.len();
    let total = total_limit(&app.plans);
    let title = if total > 0 {
        format!(
            "计划 · 共 {} 个 · 额度合计 {}",
            count,
            money::format_amount(total)
        )
    } else {
        format!("计划 · 共 {count} 个")
    };

    if count == 0 {
        render_placeholder(
            frame,
            area,
            block_with_title(true, title),
            "还没有计划",
            "按 a 新建一个 · 想控预算就填上限，只想记花销就留空",
        );
        return;
    }

    // 宽屏左右分栏（列表 | 校验），窄屏上下叠：先看计划，再看够不够
    if area.width >= 96 {
        let columns = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);
        render_plan_list(frame, columns[0], app, title);
        render_plan_check(frame, columns[1], app);
    } else {
        let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(7)]).split(area);
        render_plan_list(frame, rows[0], app, title);
        render_plan_check(frame, rows[1], app);
    }
}

/// 计划列表：名称 / 上限 / 已花 / 剩余 / 用量，选中行高亮，底部给位置与操作提示。
///
/// 没上限的计划「上限」列写 `不限`、「剩余」列写 `—`：没有额度就没有「还剩多少」这件事，
/// 显示 0 会让人以为额度用光了。
fn render_plan_list(frame: &mut Frame, area: Rect, app: &App, title: String) {
    let selected = app.selected_index(Page::Plan);
    let width = area.width.saturating_sub(2) as usize;
    // 名称 8 + 三列金额 12×3 + 列间距 4，剩下给用量条
    let bar_width = width.saturating_sub(8 + 12 * 3 + 4).clamp(MIN_BAR_WIDTH, 16);

    let rows = app
        .plans
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let chosen = index == selected;
            let limit = row.plan.limit.filter(|limit| *limit > 0);
            let limit_text = match limit {
                Some(limit) => money::format_amount(limit),
                None => "不限".to_string(),
            };
            // 有上限：剩余可能为负 → 「超 X」；没上限：没有剩余这回事
            let (remaining_text, remaining_style) = match (limit, row.remaining()) {
                (Some(_), Some(rest)) if rest < 0 => {
                    (format!("超 {}", money::format_amount(rest.abs())), BAD)
                }
                (Some(_), Some(rest)) => (money::format_amount(rest), GOOD),
                _ => ("—".to_string(), MUTED),
            };
            let bar = match limit {
                Some(limit) => share(row.spent, limit, bar_width).1,
                None => BAR_EMPTY.repeat(bar_width),
            };

            let name_line = Line::from(Span::styled(row.name().to_string(), plain()));
            let limit_line = Line::from(Span::styled(limit_text, bold(TEXT))).right_aligned();
            let spent_line = Line::from(Span::styled(
                money::format_amount(row.spent),
                Style::default().fg(if row.over() { BAD } else { TEXT }),
            ))
            .right_aligned();
            let remaining_line =
                Line::from(Span::styled(remaining_text, Style::default().fg(remaining_style)))
                    .right_aligned();

            let cells = if chosen {
                vec![
                    Cell::from(name_line),
                    Cell::from(limit_line),
                    Cell::from(spent_line),
                    Cell::from(remaining_line),
                    Cell::from(bar),
                ]
            } else {
                vec![
                    Cell::from(name_line),
                    Cell::from(limit_line),
                    Cell::from(spent_line),
                    Cell::from(remaining_line),
                    Cell::from(Span::styled(bar, Style::default().fg(ACCENT))),
                ]
            };
            Row::new(cells).style(highlight(chosen))
        })
        .collect::<Vec<_>>();

    let block = block_with_title(true, title);
    let content = block.inner(area);
    let table_area = Rect {
        height: content.height.saturating_sub(1),
        ..content
    };
    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(
        Row::new(vec![
            header_cell("计划"),
            header_cell_at("上限", Alignment::Right),
            header_cell_at("已花", Alignment::Right),
            header_cell_at("剩余", Alignment::Right),
            header_cell("用量"),
        ])
        .style(header_style()),
    )
    .column_spacing(1);

    frame.render_widget(block, area);
    render_following_table(frame, table_area, table, Some(selected));

    if content.height > 0 {
        let hint_area = Rect {
            x: content.x,
            y: content.y + content.height - 1,
            width: content.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(
                Line::from(Span::styled(
                    format!(
                        "第 {} / {} 个 · Enter 编辑 · d 删除",
                        selected + 1,
                        app.plans.len()
                    ),
                    muted(),
                ))
                .right_aligned(),
            ),
            hint_area,
        );
    }
}

/// 覆盖校验：收入 / 额度合计 / 剩余（或缺口）一句话结论 —— 有上限的计划盖不盖得住工资，看这里。
fn render_plan_check(frame: &mut Frame, area: Rect, app: &App) {
    let block = block_with_title(false, "覆盖校验");
    let inner = block.inner(area);
    let width = inner.width as usize;
    let total = total_limit(&app.plans);

    let mut lines = vec![
        stat_row(
            &[
                (
                    "收入",
                    money::format_amount(
                        app.coverage
                            .as_ref()
                            .map(|coverage| coverage.income)
                            .unwrap_or(0),
                    ),
                    plain(),
                ),
                ("额度", money::format_amount(total), bold(TEXT)),
            ],
            width,
        ),
        Line::from(Span::styled("─".repeat(width), Style::default().fg(BORDER))),
    ];

    if total == 0 {
        lines.push(Line::from(Span::styled("还没设上限", muted())));
        lines.push(Line::from(Span::styled(
            "留空上限的计划只累计，不参与校验",
            muted(),
        )));
    } else {
        match app.coverage.as_ref() {
            Some(coverage) if coverage.covers() => {
                lines.push(Line::from(vec![Span::styled(
                    "✔ 额度盖得住收入",
                    bold(GOOD),
                )]));
                lines.push(Line::from(vec![
                    Span::styled("还剩 ", muted()),
                    Span::styled(money::format_amount(coverage.remaining()), bold(GOOD)),
                ]));
            }
            Some(coverage) => {
                lines.push(Line::from(Span::styled("✖ 额度盖不住收入", bold(BAD))));
                lines.push(Line::from(vec![
                    Span::styled("差 ", muted()),
                    Span::styled(money::format_amount(coverage.shortfall()), bold(BAD)),
                ]));
            }
            None => {
                lines.push(Line::from(Span::styled("收入待汇总", muted())));
            }
        }
    }

    lines.push(Line::from(vec![Span::styled(
        "按 a 新增 · e 修改",
        muted(),
    )]));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}


// ---------------------------------------------------------------------------
// 账单
// ---------------------------------------------------------------------------

/// 账单：按「日期 + 星期」分组，一页放多少条由视口高度决定；
/// 标题带月份与笔数，底部给出位置与页码。
fn render_bills(frame: &mut Frame, area: Rect, app: &App) {
    let total = app.bills.len();
    let title = format!("账单 · {} · 共 {} 笔", time::format_month(app.month), total);

    if total == 0 {
        render_placeholder(
            frame,
            area,
            block_with_title(true, title),
            "本月还没有账单",
            "按 a 记一笔 · [ ] 换个月",
        );
        return;
    }

    let block = block_with_title(true, title);
    let content = block.inner(area);
    let width = content.width as usize;
    // 面板里最后一行留给页码，其余全部用来放「日期头 + 账单」
    let capacity = content.height.saturating_sub(1) as usize;

    // 分页与翻页共用 App 里的同一份计算，页码不会和实际内容错位；
    // 一页放几行以面板的真实高度为准（窗口刚缩放那一帧视口高度还没跟上）
    let paging = app.bill_page_with(capacity);
    let selected = app.selected_index(Page::Bills);
    let page = paging.page;
    let pages = paging.pages;
    let start = paging.start;
    let end = paging.end;
    let groups = bills_by_day(&app.bills);

    // 日期表头只在当天第一笔之前出现一次；选中行记住行号，好让窗口跟着滚
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0usize;
    let mut last_date: Option<chrono::NaiveDate> = None;
    for (offset, bill) in app.bills[start..end].iter().enumerate() {
        let index = start + offset;
        let date = bill.created_at.date_naive();
        if last_date != Some(date) {
            if let Some((_, group)) = groups.iter().find(|(day, _)| *day == date) {
                lines.push(day_header_line(date, group));
            } else {
                lines.push(day_header_line(date, &[bill]));
            }
            last_date = Some(date);
        }
        if index == selected {
            selected_line = lines.len();
        }
        lines.push(bill_line(bill, index == selected, width));
    }

    // 面板放不下整页时，窗口跟着选中行走，别让高亮掉出可视区
    let offset = if capacity > 0 && selected_line + 1 > capacity {
        selected_line + 1 - capacity
    } else {
        0
    };

    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
        content,
    );

    if content.height > 0 {
        let hint_area = Rect {
            x: content.x,
            y: content.y + content.height - 1,
            width: content.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(
                Line::from(Span::styled(
                    format!(
                        "第 {}/{} 条 · 第 {}/{} 页",
                        selected + 1,
                        total,
                        page + 1,
                        pages
                    ),
                    muted(),
                ))
                .right_aligned(),
            ),
            hint_area,
        );
    }
}

/// 按日期分组，日期倒序（新的在上），组内保持账单原有顺序。
///
/// 分页（`App::bill_page`）和渲染必须用同一份分组：页边界是按「表头 + 行」
/// 的行数切出来的，两边算法一旦不一致，页码就会和实际看到的内容错位。
pub fn bills_by_day(bills: &[BillEntity]) -> Vec<(chrono::NaiveDate, Vec<&BillEntity>)> {
    let mut groups: std::collections::BTreeMap<chrono::NaiveDate, Vec<&BillEntity>> =
        std::collections::BTreeMap::new();
    for bill in bills {
        groups
            .entry(bill.created_at.date_naive())
            .or_default()
            .push(bill);
    }
    groups.into_iter().rev().collect()
}

/// 分组表头：日期 + 星期 · N 笔 · 支出 / 收入小计。
fn day_header_line(date: chrono::NaiveDate, group: &[&BillEntity]) -> Line<'static> {
    let mut expense = 0;
    let mut income = 0;
    for bill in group {
        match bill.kind {
            BillKind::Expense => expense += bill.amount,
            BillKind::Income => income += bill.amount,
        }
    }

    let mut spans = vec![
        Span::styled(
            format!("{} {}", date.format("%Y-%m-%d"), weekday_label(date)),
            bold(ACCENT),
        ),
        Span::styled(format!(" · {} 笔", group.len()), muted()),
    ];
    if expense > 0 {
        spans.push(Span::styled(
            format!(" · 支出 {}", money::format_amount(expense)),
            muted(),
        ));
    }
    if income > 0 {
        spans.push(Span::styled(
            format!(" · 收入 {}", plus_amount(income)),
            muted(),
        ));
    }
    Line::from(spans)
}

/// 星期几：按 num_days_from_monday 取，跨月跨年都不会错位。
fn weekday_label(date: chrono::NaiveDate) -> &'static str {
    const LABELS: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    LABELS[date.weekday().num_days_from_monday() as usize]
}

/// 账单页一行：方向 · 金额 · 计划 · 分类 · 备注 · 计列。
fn bill_line(bill: &BillEntity, selected: bool, width: usize) -> Line<'static> {
    let kind_style = match bill.kind {
        BillKind::Income => bold(GOOD),
        BillKind::Expense => bold(BAD),
    };
    let sign = match bill.kind {
        BillKind::Income => "+",
        BillKind::Expense => "-",
    };
    let amount = format!("{sign}{}", money::format_amount(bill.amount));
    let flag = if bill.excluded { "×" } else { "✓" };
    let room = width.saturating_sub(5 + 13 + 11 + 11 + 3);
    let remark = truncate(bill.remark.as_deref().unwrap_or_default(), room);

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            match bill.kind {
                BillKind::Income => "收入",
                BillKind::Expense => "支出",
            },
            kind_style,
        ),
        Span::raw(" "),
        Span::styled(pad_left(&amount, 12), kind_style),
        Span::raw(" "),
        Span::styled(pad_right(&truncate(&bill.book.name, 10), 10), plain()),
        Span::raw(" "),
        Span::styled(pad_right(&truncate(&bill.category.name, 10), 10), plain()),
        Span::raw(" "),
        Span::styled(pad_right(&remark, room), muted()),
        Span::styled(flag, muted()),
    ];
    if bill.excluded {
        spans = spans
            .into_iter()
            .map(|span| span.style(Style::default().fg(MUTED)))
            .collect();
    }
    Line::from(spans).style(highlight(selected))
}

/// 概览「最近动账」的一行：备注（没有就落到分类）· 金额 · 计划。
fn compact_bill_line(bill: &BillEntity, width: usize) -> Line<'static> {
    let kind_style = match bill.kind {
        BillKind::Income => bold(GOOD),
        BillKind::Expense => bold(BAD),
    };
    let sign = match bill.kind {
        BillKind::Income => "+",
        BillKind::Expense => "-",
    };
    let amount = format!("{sign}{}", money::format_amount(bill.amount));
    let title = bill
        .remark
        .clone()
        .filter(|remark| !remark.is_empty())
        .unwrap_or_else(|| bill.category.name.clone());
    let room = width.saturating_sub(2 + 13 + 9).max(4);

    Line::from(vec![
        Span::raw("  "),
        Span::styled(pad_right(&truncate(&title, room), room), plain()),
        Span::styled(pad_left(&amount, 12), kind_style),
        Span::raw(" "),
        Span::styled(truncate(&bill.book.name, 8), muted()),
    ])
}

// ---------------------------------------------------------------------------
// 月报
// ---------------------------------------------------------------------------

/// 月报：左「汇总 + 按分类收入」、右「分类 + 计划排行 + 明细」。
fn render_months(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = app.report.as_ref() else {
        render_loading(frame, area, "月报", "正在汇总本月账单…");
        return;
    };

    let columns =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);
    // 左列「汇总」只占固定高度，把剩下的高度让给「按分类 · 收入」
    let left = Layout::vertical([Constraint::Length(6), Constraint::Min(8)]).split(columns[0]);
    // 右列 3:2：支出排行是主角，明细按内容多少自适应
    let right = Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(columns[1]);

    render_month_boxes(frame, left[0], report);
    render_income_table(frame, left[1], report);
    render_pair_table(frame, right[0], app, report);
    render_pair_detail(frame, right[1], app);
}

/// 汇总卡片：收入独占第一行（月报主角），起始 / 支出 / 结余排下面。
fn render_month_boxes(frame: &mut Frame, area: Rect, report: &MonthReport) {
    let collection = &report.collection;
    let block = block_with_title(false, "汇总");
    let inner = block.inner(area);
    let width = inner.width as usize;

    // 收入做成反色胶囊：一眼扫到的是收入，不是支出
    let chip = Style::default()
        .fg(ratatui::style::Color::Black)
        .bg(GOOD)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" 收入 ", chip),
            Span::raw(" "),
            Span::styled(plus_amount(collection.total_income), bold(GOOD)),
        ]),
        Line::from(Span::styled("─".repeat(width), Style::default().fg(BORDER))),
    ];

    // 四项一行要 78 列才排得开；窄了拆成两行两列，净额不能被裁成半截
    if width < 78 {
        lines.push(stat_row(
            &[
                (
                    "起始金额",
                    money::format_amount(collection.start_balance),
                    plain(),
                ),
                (
                    "支出",
                    money::format_amount(collection.total_expense),
                    bold(BAD),
                ),
            ],
            width,
        ));
        lines.push(stat_row(
            &[
                (
                    "结余",
                    money::format_amount(collection.remaining()),
                    bold(TEXT),
                ),
                ("净额", plus_amount(collection.net()), bold(GOOD)),
            ],
            width,
        ));
    } else {
        lines.push(stat_row(
            &[
                (
                    "起始金额",
                    money::format_amount(collection.start_balance),
                    plain(),
                ),
                (
                    "支出",
                    money::format_amount(collection.total_expense),
                    bold(BAD),
                ),
                (
                    "结余",
                    money::format_amount(collection.remaining()),
                    bold(TEXT),
                ),
                ("净额", plus_amount(collection.net()), bold(GOOD)),
            ],
            width,
        ));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// 「按分类 · 收入」：分类 / 占比 / 构成（条形图）。
fn render_income_table(frame: &mut Frame, area: Rect, report: &MonthReport) {
    let collection = &report.collection;
    let title = "按分类 · 收入";
    let inner_width = area.width.saturating_sub(4) as usize;
    let bar_width = (inner_width.saturating_sub(8 + 7 + 2)).clamp(4, 12);

    let mut entries: Vec<(u64, AmountType)> = collection
        .income_by_category
        .iter()
        .map(|(id, amount)| (*id, *amount))
        .collect();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));

    if entries.is_empty() {
        render_placeholder(
            frame,
            area,
            block_with_title(false, title),
            "本月还没有收入",
            "按 a 记一笔，方向选「收入」",
        );
        return;
    }

    let total = collection.total_income;
    let rows = entries
        .iter()
        .map(|(id, amount)| {
            let name = name_of(&report.category_names, *id);
            let (share_text, bar) = share(*amount, total, bar_width);
            Row::new(vec![
                Cell::from(Span::styled(name, plain())),
                Cell::from(Line::from(Span::styled(share_text, muted())).right_aligned()),
                Cell::from(Span::styled(bar, Style::default().fg(GOOD))),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(
        Row::new(vec![
            header_cell("分类"),
            header_cell_at("占比", Alignment::Right),
            header_cell("构成"),
        ])
        .style(header_style()),
    )
    .column_spacing(1)
    .block(block_with_title(false, title));

    frame.render_widget(table, area);
}

/// 「支出 · 分类 + 计划」：排行 + 占比 + 条形图；选中行跟着明细走。
fn render_pair_table(frame: &mut Frame, area: Rect, app: &App, report: &MonthReport) {
    let collection = &report.collection;
    let pairs = collection.expense_pairs();
    let total = collection.total_expense;
    let selected = app.selected_index(Page::Reports);
    let title = format!(
        "支出 · 分类 + 计划 · 合计 {}{}",
        money::format_amount(total),
        position_hint(selected, pairs.len())
    );

    if pairs.is_empty() {
        render_placeholder(
            frame,
            area,
            block_with_title(true, title),
            "本月还没有支出",
            "按 a 记一笔，方向选「支出」",
        );
        return;
    }

    let inner_width = area.width.saturating_sub(4) as usize;
    let bar_width = (inner_width.saturating_sub(8 + 8 + 12 + 6 + 4)).clamp(4, MAX_BAR_WIDTH);

    let rows = pairs
        .iter()
        .enumerate()
        .map(|(index, (category_id, book_id, amount))| {
            let chosen = index == selected;
            let rank = match index {
                0 => BAD,
                1 => WARN,
                2 => ACCENT,
                _ => MUTED,
            };
            let name = name_of(&report.category_names, *category_id);
            let book = name_of(&report.book_names, *book_id);
            let (share_text, bar) = share(*amount, total, bar_width);

            let cells = if chosen {
                vec![
                    Cell::from(name),
                    Cell::from(book),
                    Cell::from(Line::from(money::format_amount(*amount)).right_aligned()),
                    Cell::from(Line::from(share_text).right_aligned()),
                    Cell::from(bar),
                ]
            } else {
                vec![
                    Cell::from(Span::styled(name, plain())),
                    Cell::from(Span::styled(book, plain())),
                    Cell::from(
                        Line::from(Span::styled(money::format_amount(*amount), bold(rank)))
                            .right_aligned(),
                    ),
                    Cell::from(Line::from(Span::styled(share_text, muted())).right_aligned()),
                    Cell::from(Span::styled(bar, Style::default().fg(rank))),
                ]
            };

            Row::new(cells).style(highlight(chosen))
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(
        Row::new(vec![
            header_cell("分类"),
            header_cell("计划"),
            header_cell_at("金额", Alignment::Right),
            header_cell_at("占比", Alignment::Right),
            header_cell("构成"),
        ])
        .style(header_style()),
    )
    .column_spacing(1)
    .block(block_with_title(false, title));

    render_following_table(frame, area, table, Some(selected));
}

/// 明细：当前选中「分类 + 计划」在本月的账单。
fn render_pair_detail(frame: &mut Frame, area: Rect, app: &App) {
    let label = app
        .month_selected_label()
        .unwrap_or_else(|| "还没选中排行".to_string());
    let title = format!("明细 · {label}");
    let bills = app.month_selected_bills();

    if bills.is_empty() {
        render_placeholder(
            frame,
            area,
            block_with_title(false, title),
            "这个分类 + 计划本月没有账单",
            "↑ ↓ 换一行看看",
        );
        return;
    }

    let rows = bills
        .iter()
        .map(|bill| {
            Row::new(vec![
                Cell::from(Span::styled(
                    bill.created_at.date_naive().to_string(),
                    muted(),
                )),
                Cell::from(
                    Line::from(Span::styled(
                        money::format_amount(bill.amount),
                        bold(match bill.kind {
                            BillKind::Income => GOOD,
                            BillKind::Expense => BAD,
                        }),
                    ))
                    .right_aligned(),
                ),
                Cell::from(Span::styled(
                    bill.remark.clone().unwrap_or_default(),
                    plain(),
                )),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(6),
        ],
    )
    .header(
        Row::new(vec![
            header_cell("日期"),
            header_cell_at("金额", Alignment::Right),
            header_cell("备注"),
        ])
        .style(header_style()),
    )
    .column_spacing(1)
    .block(block_with_title(false, title));

    frame.render_widget(table, area);
}

// ---------------------------------------------------------------------------
// 年报
// ---------------------------------------------------------------------------

/// 年报：顶部一行总账 → 中间按计划两张表 → 底部逐月走势。
fn render_years(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = app.year_report.as_ref() else {
        render_loading(frame, area, "年报", "正在汇总全年账单…");
        return;
    };
    let collection = &report.collection;

    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(15),
    ])
    .split(area);

    render_year_summary(frame, rows[0], app, collection);
    let books =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[1]);
    render_book_table(
        frame,
        books[0],
        "按计划 · 收入",
        &collection.income_by_book,
        &report.book_names,
        collection.total_income,
        GOOD,
        false,
    );
    render_book_table(
        frame,
        books[1],
        "按计划 · 支出",
        &collection.expense_by_book,
        &report.book_names,
        collection.total_expense,
        BAD,
        true,
    );
    render_monthly_table(frame, rows[2], app, collection);
}

/// 年报顶行：收入 │ 支出 │ 净额（正绿负红）。
fn render_year_summary(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    collection: &crate::month::model::YearCollection,
) {
    let block = block_with_title(false, format!("{} 年报 · [ ] 换年", app.year));
    let inner = block.inner(area);
    let net = collection.net();
    let line = stat_row(
        &[
            (
                "收入",
                money::format_amount(collection.total_income),
                bold(GOOD),
            ),
            (
                "支出",
                money::format_amount(collection.total_expense),
                bold(BAD),
            ),
            (
                "净额",
                plus_amount(net),
                bold(if net < 0 { BAD } else { GOOD }),
            ),
        ],
        inner.width as usize,
    );
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// 按计划汇总：计划名 / 金额 / 占比（支出再多一列分布条）。
#[allow(clippy::too_many_arguments)]
fn render_book_table(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    values: &HashMap<u64, AmountType>,
    names: &HashMap<u64, String>,
    total: AmountType,
    color: Color,
    with_bar: bool,
) {
    let block = block_with_title(false, title);
    let mut entries: Vec<(u64, AmountType)> =
        values.iter().map(|(id, amount)| (*id, *amount)).collect();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));

    if entries.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("今年还没有记录", muted())))
                .block(block_with_title(false, title))
                .centered(),
            area,
        );
        let _ = inner;
        return;
    }

    let inner_width = area.width.saturating_sub(4) as usize;
    let fixed = if with_bar {
        8 + 12 + 7 + 3
    } else {
        8 + 12 + 7 + 2
    };
    // 支出那张表带分布条，同样收到 12 列，两张表的数字列才对得齐
    let bar_width = inner_width.saturating_sub(fixed).clamp(MIN_BAR_WIDTH, 12);

    let rows = entries
        .iter()
        .map(|(id, amount)| {
            let name = name_of(names, *id);
            let (share_text, bar) = share(*amount, total, bar_width);
            let mut cells = vec![
                Cell::from(Span::styled(name, plain())),
                Cell::from(
                    Line::from(Span::styled(money::format_amount(*amount), bold(color)))
                        .right_aligned(),
                ),
                Cell::from(Line::from(Span::styled(share_text, muted())).right_aligned()),
            ];
            if with_bar {
                cells.push(Cell::from(Span::styled(bar, Style::default().fg(color))));
            }
            Row::new(cells)
        })
        .collect::<Vec<_>>();

    let mut header = vec![
        header_cell("计划"),
        header_cell_at("金额", Alignment::Right),
        header_cell_at("占比", Alignment::Right),
    ];
    if with_bar {
        header.push(header_cell("支出分布"));
    }

    let widths: Vec<Constraint> = if with_bar {
        vec![
            Constraint::Min(8),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(bar_width as u16),
        ]
    } else {
        vec![
            Constraint::Min(8),
            Constraint::Length(12),
            Constraint::Length(7),
        ]
    };

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(header_style()))
        .column_spacing(1)
        .block(block);

    frame.render_widget(table, area);
}

/// 逐月走势：12 行，当前月高亮，带支出分布条。
fn render_monthly_table(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    collection: &crate::month::model::YearCollection,
) {
    let peak = collection
        .expense_by_month
        .iter()
        .copied()
        .max()
        .unwrap_or(0);
    let peak_month = collection
        .expense_by_month
        .iter()
        .position(|amount| *amount == peak)
        .map(|index| index + 1)
        .unwrap_or(1);
    let title = format!("逐月走势 · 支出最高 {peak_month} 月");
    let inner_width = area.width.saturating_sub(4) as usize;
    // 分布条收到 12 列：条形只是辅助，不该把宽度从数字列手里抢走
    let bar_width = inner_width
        .saturating_sub(8 + 14 + 14 + 14 + 4)
        .clamp(MIN_BAR_WIDTH, 12);

    // 没数据的月份默认折叠：一年里十一个月的 0.00 只会淹没真正有数的那几行
    let visible: Vec<usize> = (0..12)
        .filter(|index| {
            app.year_expanded
                || collection.income_by_month[*index] != 0
                || collection.expense_by_month[*index] != 0
        })
        .collect();
    let folded = 12 - visible.len();

    let rows = visible
        .iter()
        .map(|index| {
            let income = collection.income_by_month[*index];
            let expense = collection.expense_by_month[*index];
            let net = income - expense;
            let current =
                collection.year == app.month.year() && *index + 1 == app.month.month() as usize;
            // 有数的行里，当前月份用 ACCENT 标出来；其余走常规色
            let base = if current {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                plain()
            };
            let net_style = if current {
                base
            } else {
                Style::default().fg(if net < 0 { BAD } else { GOOD })
            };
            let mut cells = vec![
                Cell::from(Span::styled(format!("{} 月", *index + 1), base)),
                Cell::from(
                    Line::from(Span::styled(money::format_amount(income), base)).right_aligned(),
                ),
                Cell::from(
                    Line::from(Span::styled(money::format_amount(expense), base)).right_aligned(),
                ),
                Cell::from(Line::from(Span::styled(plus_amount(net), net_style)).right_aligned()),
            ];
            if expense == 0 {
                cells.push(Cell::from(Span::styled("·", muted())));
            } else {
                let (_, bar) = share(expense, peak.max(1), bar_width);
                cells.push(Cell::from(Span::styled(
                    bar,
                    Style::default().fg(if current { ACCENT } else { BAD }),
                )));
            }
            Row::new(cells)
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(bar_width as u16),
        ],
    )
    .header(
        Row::new(vec![
            header_cell("月份"),
            header_cell_at("收入", Alignment::Right),
            header_cell_at("支出", Alignment::Right),
            header_cell_at("净额", Alignment::Right),
            header_cell("支出分布"),
        ])
        .style(header_style()),
    )
    .column_spacing(1)
    .block(block_with_title(true, title));

    frame.render_widget(table, area);

    // 折叠提示贴在表格下边框内侧，不另占一整行面板
    let hint = if folded == 0 {
        None
    } else if app.year_expanded {
        Some("空月份已全部展开 · 按 x 收起".to_string())
    } else {
        Some(format!("另有 {folded} 个月没有记录 · 按 x 展开全部"))
    };
    if let Some(hint) = hint {
        let hint_area = Rect {
            x: area.x + 2,
            y: area.y + area.height.saturating_sub(2),
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, muted()))),
            hint_area,
        );
    }
}

// ---------------------------------------------------------------------------
// 底部状态栏
// ---------------------------------------------------------------------------

/// 底栏：第 1 行状态，第 2 行上下文快捷键（按键说话，按页面换）。
fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    // 状态行只有一个位置：出错 > 刚才做了什么 > 有待核对的流水 > 就绪
    let (icon, message, style) = match app.status.as_ref() {
        Some(status) if status.is_error() => ("✖", status.text().to_string(), bold(BAD)),
        Some(status) => ("✔", status.text().to_string(), bold(GOOD)),
        None if !app.pending_sync.is_empty() => (
            "◦",
            format!("待核对 {} 笔 · Enter 继续", app.pending_sync.len()),
            bold(WARN),
        ),
        None => ("◦", "就绪".to_string(), muted()),
    };
    let right = format!("{} · {}", app.page.title(), time::format_month(app.month));
    let mut left: Vec<Span<'static>> = vec![
        Span::styled(format!("{icon} "), style),
        Span::styled(message, style),
    ];
    let left_width: usize = left
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum();
    let gap = (rows[0].width as usize)
        .saturating_sub(left_width)
        .saturating_sub(display_width(&right));
    if gap >= 1 {
        left.push(Span::raw(" ".repeat(gap)));
    }
    left.push(Span::styled(right, muted()));
    frame.render_widget(Paragraph::new(Line::from(left)), rows[0]);

    // 第 2 行：上下文快捷键，超过 6 组就砍尾部；完整键位按 `?` 看
    let keys = hotkeys_for(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (key, hint)) in keys.iter().take(6).enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", muted()));
        }
        spans.push(Span::styled((*key).to_string(), bold(ACCENT)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled((*hint).to_string(), muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rows[1]);
}

/// 当前页面 / 弹窗下真正可用的快捷键。
///
/// 全局约定：`↑ ↓` 管页内、`← →` 管页签（`Tab` 也翻页签，只是不占提示位）。
fn hotkeys_for(app: &App) -> Vec<(&'static str, &'static str)> {
    if let Some(modal) = app.modal.as_ref() {
        return match modal {
            Modal::Form(_) => vec![
                ("Enter", "提交"),
                ("空格 / ← →", "改选择"),
                ("Tab", "下一项"),
                ("Esc", "取消"),
            ],
            Modal::Picker { .. } => vec![("↑ ↓", "选择"), ("Enter", "确认"), ("Esc", "取消")],
            Modal::Confirm { .. } => vec![("y / Enter", "确认"), ("n / Esc", "取消")],
            Modal::Notice { .. } => vec![("任意键", "关闭")],
            Modal::Help => vec![("任意键", "关闭")],
        };
    }

    match app.page {
        Page::Overview => {
            if !app.pending_sync.is_empty() {
                // 有没核完的流水时，Enter 是最该被看见的第一个键
                return vec![
                    ("Enter", "继续核对"),
                    ("a", "记一笔"),
                    ("?","帮助"),
                    ("← →", "切页"),
                ];
            }
            // 长尾键位（w / e / m / q）挪进 `?` 帮助，底栏只留高频的五个
            vec![
                ("↑ ↓", "换日"),
                ("a", "记一笔"),
                ("s", "同步今日"),
                ("?","帮助"),
                ("← →", "切页"),
            ]
        }
        Page::Plan => vec![
            ("↑ ↓", "选择"),
            ("Enter", "改这块"),
            ("a", "新增一块"),
            ("d", "删除"),
            ("?","帮助"),
        ],
        Page::Bills => vec![
            ("↑ ↓", "选择"),
            ("a", "记一笔"),
            ("Enter", "编辑"),
            ("d", "删除"),
            ("x", "计列"),
            ("?","帮助"),
        ],
        Page::Reports => match app.report_tab {
            ReportTab::Month => vec![
                ("↑ ↓", "选择"),
                ("a", "记一笔"),
                ("[  ]", "换月"),
                ("y", "年报"),
                ("?","帮助"),
            ],
            ReportTab::Year => vec![
                ("[  ]", "换年"),
                ("x", "展开月份"),
                ("m", "月报"),
                ("?","帮助"),
            ],
        },
        Page::Settings => match app.settings_focus {
            // 连接分区没有列表可进，Enter 换成直接改连接，别让提示说谎
            SettingsFocus::Menu if app.settings_section == SettingsSection::Connection => vec![
                ("↑ ↓", "选分区"),
                ("e", "改连接"),
                ("i", "初始化"),
                ("t", "迁移"),
                ("?","帮助"),
            ],
            SettingsFocus::Menu => vec![
                ("↑ ↓", "选分区"),
                ("Enter", "进列表"),
                ("a", "新增"),
                ("?","帮助"),
            ],
            SettingsFocus::Detail => vec![
                ("↑ ↓", "选条目"),
                ("Enter", "编辑"),
                ("a", "新增"),
                ("d", "删除"),
                ("Esc", "回菜单"),
                ("?","帮助"),
            ],
        },
    }
}
// ---------------------------------------------------------------------------
// 弹窗
// ---------------------------------------------------------------------------

fn render_modal(frame: &mut Frame, app: &App) {
    let Some(modal) = app.modal.as_ref() else {
        return;
    };

    match modal {
        Modal::Form(form) => {
            render_form_modal(frame, form);
        }
        Modal::Picker {
            form,
            label,
            options,
            selected,
            ..
        } => {
            // 列表只是「表单里的一次选择」：表单留原处，列表贴到它旁边，
            // 这样打开候选时表单的边框、已填内容都还看得见。
            let form_rect = render_form_modal(frame, form);
            render_picker_modal(frame, label, options, *selected, form_rect);
        }
        Modal::Confirm { title, message, .. } => render_confirm_modal(frame, title, message),
        Modal::Notice { title, message } => render_notice_modal(frame, title, message),
        Modal::Help => render_help_modal(frame),
    }
}

/// 表单弹窗：标签右对齐、当前行 `▶`、底部一行操作提示。
fn render_form_modal(frame: &mut Frame, form: &Form) -> Rect {
    let screen = frame.area();
    let label_width = form
        .fields
        .iter()
        .map(|field| display_width(field.label))
        .max()
        .unwrap_or(0);
    let width = (label_width + 24 + 6) as u16;
    let width = width.clamp(30, 64).min(screen.width.saturating_sub(4));
    let value_width = (width as usize).saturating_sub(label_width + 10);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        let marker_style = if focused { bold(ACCENT) } else { muted() };
        let value_style = if focused { bold(TEXT) } else { plain() };

        let mut spans = vec![
            Span::styled(if focused { "▶ " } else { "  " }, marker_style),
            Span::styled(pad_left(field.label, label_width), muted()),
            Span::styled("  ", muted()),
            Span::styled(truncate(&field.value, value_width), value_style),
        ];
        if field.is_choice() {
            spans.push(Span::styled(" ▾", muted()));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Enter 保存 · Esc 取消", muted())));

    let height = lines.len() as u16 + 2;
    let rect = centered_rect(screen, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(block_with_title(true, form.title)),
        rect,
    );
    rect
}

/// 选择弹窗：一行一个候选，`▶` 标当前项。
fn render_picker_modal(
    frame: &mut Frame,
    label: &str,
    options: &[String],
    selected: usize,
    anchor: Rect,
) {
    let screen = frame.area();
    let widest = options
        .iter()
        .map(|option| display_width(option))
        .max()
        .unwrap_or(0);
    // 底部那行操作提示也得放得下，否则会被右边界裁掉半句。
    let hint = "↑ ↓ 选择 · Enter 确认 · Esc 取消";
    let width = (widest + 12).max(display_width(hint) + 8) as u16;
    let width = width.clamp(32, 60).min(screen.width.saturating_sub(4));
    let inner = (width as usize).saturating_sub(6);

    // 选项多到一屏放不下时只画一个跟着选中项滚动的窗口。
    const MAX_ROWS: usize = 12;
    let visible = options.len().min(MAX_ROWS);
    let start = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (offset, option) in options.iter().skip(start).take(visible).enumerate() {
        let index = start + offset;
        let current = index == selected;
        let marker = if current { "▶ " } else { "  " };
        let style = if current {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            plain()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, if current { bold(ACCENT) } else { muted() }),
            Span::styled(pad_right(&truncate(option, inner), inner), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(hint, muted())));

    if options.len() > visible {
        lines.push(Line::from(Span::styled(
            format!("共 {} 项，继续 ↑ ↓ 滚动", options.len()),
            muted(),
        )));
    }

    let height = lines.len() as u16 + 2;
    let rect = place_beside(screen, anchor, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(block_with_title(true, label.to_string())),
        rect,
    );
}

/// 把弹窗尽量放到 `anchor`（表单）的右侧，放不下就放下方，都放不下才居中。
///
/// 关键是不要压住表单：压住了表单的左边框就"消失"了。
fn place_beside(screen: Rect, anchor: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(screen.width.saturating_sub(2)).max(10);
    let height = height.min(screen.height.saturating_sub(2)).max(3);
    let bottom = screen.y + screen.height;

    // 右侧
    let right_x = anchor.x.saturating_add(anchor.width).saturating_add(2);
    if right_x + width <= screen.x + screen.width {
        return Rect {
            x: right_x,
            y: anchor.y.min(bottom.saturating_sub(height)),
            width,
            height,
        };
    }

    // 下方
    let below_y = anchor.y.saturating_add(anchor.height).saturating_add(1);
    if below_y + height <= bottom {
        return Rect {
            x: anchor.x.min(screen.x + screen.width - width),
            y: below_y,
            width,
            height,
        };
    }

    // 窄屏两边都放不下：至少贴着右边框，别再压掉表单的左边框。
    Rect {
        x: screen.x + screen.width - width,
        y: anchor.y.min(bottom.saturating_sub(height)).max(screen.y),
        width,
        height,
    }
}

/// 确认弹窗：一句话说清楚「删的是谁」。
fn render_confirm_modal(frame: &mut Frame, title: &str, message: &str) {
    let screen = frame.area();
    let width = (display_width(message) + 10) as u16;
    let width = width.clamp(34, 60).min(screen.width.saturating_sub(4));

    let lines = vec![
        Line::from(Span::styled(message.to_string(), bold(TEXT))).centered(),
        Line::from(""),
        Line::from(Span::styled("Enter / y 确认 · Esc / n 取消", muted())).centered(),
    ];

    let height = lines.len() as u16 + 2;
    let rect = centered_rect(screen, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(block_with_title(true, title.to_string())),
        rect,
    );
}

/// 通知弹窗：只报结果，不要求选择。
fn render_notice_modal(frame: &mut Frame, title: &str, message: &str) {
    let screen = frame.area();
    let width = (display_width(message) + 10) as u16;
    let width = width.clamp(34, 64).min(screen.width.saturating_sub(4));

    let lines = vec![
        Line::from(Span::styled(message.to_string(), plain())),
        Line::from(""),
        Line::from(Span::styled("按 Enter 关闭", muted())),
    ];

    let height = lines.len() as u16 + 2;
    let rect = centered_rect(screen, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block_with_title(true, title.to_string())),
        rect,
    );
}

/// 帮助弹窗：全量快捷键 + 概念解释。平时不占版面，`?` 才出现。
///
/// 内容按「先通用、再概念」排：按键是查得到的，概念是查不到才最需要解释的。
fn render_help_modal(frame: &mut Frame) {
    let screen = frame.area();

    let section = |title: &'static str| Line::from(Span::styled(title.to_string(), bold(ACCENT)));
    // 键名列按显示宽度补齐（箭头、中文都是双宽，不能用字符数算）
    let row = |key: &'static str, hint: &'static str| {
        let pad = " ".repeat(16usize.saturating_sub(display_width(key)));
        Line::from(vec![
            Span::styled(key.to_string(), bold(TEXT)),
            Span::raw(pad),
            Span::styled(hint.to_string(), muted()),
        ])
    };

    // 两栏：左边「怎么按键」，右边「这些词什么意思」。
    // 排成一栏能有三十多行，40 行的终端里会顶到上下边缘，反而更乱。
    let left = vec![
        section("通用"),
        row("1-4 / ← →", "切页（概览 / 计划 / 账单 / 报表）"),
        row(",", "开关设置页"),
        row("a", "记一笔"),
        row("e", "改当前选中的东西"),
        row("[ ]", "换月；年报时换年"),
        row("m", "回到本月；报表切回月报"),
        row("r", "刷新"),
        row("q", "退出"),
        Line::from(""),
        section("概览"),
        row("↑ ↓ / PgUp PgDn", "换日期（一天 / 一周）"),
        row("s / w", "同步今日 / 本周流水"),
        row("Enter", "继续核对没核完的流水"),
        Line::from(""),
        section("报表"),
        row("m / y", "月报 / 年报"),
        row("x", "年报：展开空月份"),
    ];
    let right = vec![
        section("计划"),
        row("a", "新增一块，名称自己起"),
        row("e", "改当前块，可绑一个分类"),
        row("绑分类", "这块的已花自动从账单算"),
        row("算式", "金额能写 9000/3 这种"),
        row("覆盖", "合计 ≤ 收入，超了标红"),
        row("日历", "按合计当本月上限"),
        Line::from(""),
        section("这几个词是什么意思"),
        row("额度", "本月计划分给消费的总额"),
        row("已花", "本月计入统计的支出"),
        row("还剩", "额度 − 已花，负数就是超支"),
        row("日额度", "到这天为止该花掉的累计额度"),
    ];

    let widest = |lines: &[Line]| lines.iter().map(|line| line.width()).max().unwrap_or(0) as u16;
    let left_width = widest(&left) + 2;
    let right_width = widest(&right) + 2;
    let height = (left.len().max(right.len()) as u16 + 3).min(screen.height.saturating_sub(2));

    let width = (left_width + right_width + 4).min(screen.width.saturating_sub(4));
    let rect = centered_rect(screen, width, height);
    let inner = block_with_title(true, "帮助 · 全部快捷键").inner(rect);
    let columns =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Min(right_width)])
            .split(inner);

    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(left), columns[0]);
    frame.render_widget(Paragraph::new(right), columns[1]);
    // 底部一行提示，贴在弹窗下边框内侧
    let hint = Rect {
        y: rect.y + rect.height.saturating_sub(2),
        height: 1,
        ..rect
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("  任意键关闭", muted()))).alignment(Alignment::Right),
        hint,
    );
    frame.render_widget(block_with_title(true, "帮助 · 全部快捷键"), rect);
}


/// 在屏幕中央开一个固定大小的弹窗（给边框留 2 列 / 2 行余量）。
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(10);
    let height = height.min(area.height.saturating_sub(2)).max(3);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// 共享小工具
// ---------------------------------------------------------------------------

/// 一行统计：每组「标签 MUTED + 金额右对齐」，组间用 ` │ ` 分隔、按宽度均分。
fn stat_row(items: &[(&str, String, Style)], width: usize) -> Line<'static> {
    let count = items.len().max(1);
    let separator = 3usize;
    let column = width.saturating_sub(separator * count.saturating_sub(1)) / count;

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (label, value, style)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", muted()));
        }
        let pad = column.saturating_sub(display_width(label) + 1 + display_width(value));
        spans.push(Span::styled((*label).to_string(), muted()));
        spans.push(Span::raw(" "));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(value.clone(), *style));
    }
    Line::from(spans)
}

/// 表头单元格：MUTED + BOLD，数字列可换右对齐。
fn header_cell(text: &str) -> Cell<'static> {
    header_cell_at(text, Alignment::Left)
}

fn header_cell_at(text: &str, alignment: Alignment) -> Cell<'static> {
    let line = Line::from(Span::styled(text.to_string(), header_style())).alignment(alignment);
    Cell::from(line)
}

fn header_style() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
}

/// 选中行整行反色：ACCENT 底 + 黑字（数字语义色在选中时让位给「这是选中行」）。
fn highlight(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        plain()
    }
}

/// 「· 第 i/n」；空列表返回空串，标题里自然不出现。
fn position_hint(selected: usize, total: usize) -> String {
    if total == 0 {
        String::new()
    } else {
        format!(" · 第 {}/{}", selected + 1, total)
    }
}

/// 渲染一张带选中状态的表格；选中行在可视区外时自动滚动。
fn render_following_table(
    frame: &mut Frame,
    area: Rect,
    table: Table<'static>,
    selected: Option<usize>,
) {
    let mut state = TableState::new().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

/// 占比 + 条形图：`(百分比文本, bar)`。
fn share(amount: AmountType, total: AmountType, bar_width: usize) -> (String, String) {
    if total <= 0 || bar_width == 0 {
        return ("0.0%".to_string(), BAR_EMPTY.repeat(bar_width));
    }
    let ratio = amount as f64 / total as f64;
    let filled = ((ratio * bar_width as f64).round() as usize).min(bar_width);
    (
        format!("{:.1}%", ratio * 100.0),
        format!(
            "{}{}",
            BAR_FULL.repeat(filled),
            BAR_EMPTY.repeat(bar_width.saturating_sub(filled))
        ),
    )
}

/// 金额的「带正号」写法，净额/结余用它一眼看出方向。
fn plus_amount(cents: AmountType) -> String {
    if cents > 0 {
        format!("+{}", money::format_amount(cents))
    } else {
        money::format_amount(cents)
    }
}

/// 百分比：分母为 0 时显示 `—`。
/// 名字查表，缺了也给出可读的占位而不是 panic。
fn name_of(names: &HashMap<u64, String>, id: u64) -> String {
    names
        .get(&id)
        .cloned()
        .unwrap_or_else(|| format!("(已删除 id={id})"))
}

/// 当月第一天是周几（周一 = 0）与当月天数。
fn month_shape(year: i32, month: u32) -> (u32, u32) {
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1);
    let next = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let leading = first
        .map(|date| date.weekday().num_days_from_monday())
        .unwrap_or(0);
    let days = match (first, next) {
        (Some(start), Some(end)) => (end - start).num_days() as u32,
        _ => 30,
    };
    (leading, days)
}

/// 星期几的中文名。
fn weekday_name(month: chrono::NaiveDate, day: u32) -> &'static str {
    let labels = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    chrono::NaiveDate::from_ymd_opt(month.year(), month.month(), day)
        .map(|date| labels[date.weekday().num_days_from_monday() as usize % 7])
        .unwrap_or("")
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
    use crate::bill_book::model::Plan;
    use crate::plan::model::{PlanProgress, PlanRow};
    use crate::tui::app::{Action, Form, Modal, Page, SettingsSection};
    use crate::tui::input::Key;
    use chrono::{NaiveDate, TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_app() -> App {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut app = App::new(
            month,
            "postgres://postgres:postgres@127.0.0.1:5432/biller",
            "C:/Users/test/.biller/biller.db",
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

        // 概览页的日历需要计划进度：有上限的计划合计 4000.00，5 号花了 12.34
        app.plans = vec![PlanRow::new(
            Plan::new(
                BillBook {
                    id: 1,
                    name: "日常开销".into(),
                },
                Some(4_000_00),
            ),
            1234,
        )];
        let mut spending = HashMap::new();
        spending.insert(5, 1234);
        app.progress = Some(PlanProgress::build(month, 4_000_00, None, &spending));

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
                    let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
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
            assert!(
                screen.contains(page.title()),
                "页签应包含「{}」：\n{screen}",
                page.title()
            );
        }
    }

    #[test]
    fn overview_page_focuses_on_today() {
        let app = sample_app();
        let screen = render_screen(&app, 100, 26);

        // 一屏三个数字：已花 / 额度 / 还剩
        assert!(screen.contains("已花"), "{screen}");
        assert!(screen.contains("4,000.00"), "应显示本月额度：\n{screen}");

        // 收入、起始金额、结余、净额都是月报的活儿，概览不再重复一遍
        for needle in ["本月概览", "起始金额", "净额", "计划执行"] {
            assert!(
                !screen.contains(needle),
                "概览不该再出现「{needle}」：\n{screen}"
            );
        }

        // 今天那块：标题带日期与今日额度，正文只有两个数字
        assert!(screen.contains("今日额度"), "{screen}");
        assert!(screen.contains("今日已花"), "{screen}");
        assert!(screen.contains("今日还剩"), "{screen}");

        // 日历：图例搬进边框标题，页内不再重复快捷键
        assert!(screen.contains("支出日历"), "概览应有支出日历：\n{screen}");
        assert!(screen.contains("超日额度"), "日历标题应有图例：\n{screen}");
        assert!(
            !screen.contains("↑ ↓ 换日期"),
            "快捷键只留底栏一处：\n{screen}"
        );
    }

    #[test]
    fn bills_page_lists_rows() {
        let mut app = sample_app();
        app.page = Page::Bills;
        let screen = render_screen(&app, 120, 24);
        assert!(
            screen.contains("2026-10-05"),
            "时间应只展示到天：\n{screen}"
        );
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
    fn tall_bill_page_fills_the_viewport_instead_of_stopping_at_ten() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.bills = many_bills(9);
        app.selected[Page::Bills.index()] = 0;

        // 40 行终端能放下 9 笔 + 1 个日期头：下半屏不许再空着
        let screen = render_screen(&app, 120, 40);
        assert!(screen.contains("第1笔"), "{screen}");
        assert!(
            screen.contains("第9笔"),
            "高终端一页应放下全部 9 笔，而不是停在 10 条线（这里只有 9 笔）：\n{screen}"
        );
        assert!(
            screen.contains("第 1/9 条 · 第 1/1 页"),
            "一页装得下就不该多分页：\n{screen}"
        );
    }

    #[test]
    fn plan_page_survives_a_narrow_terminal() {
        let mut app = sample_app();
        app.page = Page::Plan;
        app.plans = vec![
            PlanRow::new(
                Plan::new(
                    BillBook {
                        id: 1,
                        name: "吃饭".into(),
                    },
                    Some(1_500_00),
                ),
                88_50,
            ),
            PlanRow::new(
                Plan::new(
                    BillBook {
                        id: 2,
                        name: "存款".into(),
                    },
                    None,
                ),
                9_000_00,
            ),
        ];

        // 窄屏走上下叠布局，表格列会被压缩，但不能渲染失败、也不能丢列名
        let screen = render_screen(&app, 76, 30);
        assert!(screen.contains("上限"), "{screen}");
        assert!(screen.contains("已花"), "{screen}");
        assert!(screen.contains("吃饭"), "{screen}");
        assert!(screen.contains("不限"), "没上限的计划要写清楚：\n{screen}");
    }

    #[test]
    fn settings_page_hosts_categories_and_connection() {
        let mut app = sample_app();
        app.page = Page::Settings;

        // 分类分区（树状缩进）
        app.settings_section = SettingsSection::Categories;
        let screen = render_screen(&app, 110, 26);
        assert!(screen.contains("▶ 分类"), "当前分区应有记号：\n{screen}");
        assert!(screen.contains("餐饮"), "{screen}");
        assert!(screen.contains("午餐"), "{screen}");

        // 运维分区不该出现在首屏（默认就停在分类上）
        assert!(!screen.contains("连接串"), "首屏不该铺数据库表单：\n{screen}");

        // 换分区不该改变页面
        assert_eq!(app.page, Page::Settings);
    }

    #[test]
    fn connection_form_renders_both_modes() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Connection;

        // 远程模式：模式预选 PostgreSQL，PG 各项都在
        app.on_key(Key::Char('e'));
        let screen = render_screen(&app, 120, 44);
        assert!(screen.contains("模式"), "应有模式选择：\n{screen}");
        assert!(screen.contains("PostgreSQL"), "{screen}");
        assert!(
            screen.contains("文件路径"),
            "本地文件路径也要能填：\n{screen}"
        );

        // 本地模式：预选本地文件，并停在文件路径那一行
        app.modal = None;
        app.connection_url = "sqlite:C:/Users/test/.biller/biller.db".to_string();
        app.on_key(Key::Char('e'));
        let screen = render_screen(&app, 120, 44);
        assert!(screen.contains("本地文件"), "{screen}");
        assert!(screen.contains("biller.db"), "{screen}");
    }

    #[test]
    fn setup_screen_shows_when_no_database() {
        let mut app = sample_app();
        app.mark_needs_setup();

        let screen = render_screen(&app, 110, 24);
        assert!(
            screen.contains("还没有可用的数据库连接"),
            "没库时应显示引导页：\n{screen}"
        );
        assert!(
            screen.contains("sqlite:C:/Users"),
            "应给出本地文件的写法示例：\n{screen}"
        );

        // 设置页必须放行，否则用户连配库的入口都没有
        app.page = Page::Settings;
        let screen = render_screen(&app, 110, 24);
        assert!(
            !screen.contains("还没有可用的数据库连接"),
            "设置页不该被引导页挡住：\n{screen}"
        );
        assert!(screen.contains("数据库连接"), "{screen}");
    }

    #[test]
    fn months_page_compares_category_with_book() {
        let mut app = sample_app();
        app.page = Page::Reports;
        let screen = render_screen(&app, 120, 30);

        assert!(screen.contains("支出 · 分类 + 计划"), "{screen}");
        assert!(
            screen.contains("按分类 · 收入"),
            "收入仍按分类汇总：\n{screen}"
        );
        // 同一行里既有分类又有计划，大头藏不住
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
            !screen.contains("按计划"),
            "月报不做按计划的独立汇总，计划口径归年报：\n{screen}"
        );
    }

    #[test]
    fn years_page_groups_by_book() {
        let mut app = sample_app();
        app.page = Page::Reports;
        app.report_tab = ReportTab::Year;
        let screen = render_screen(&app, 120, 30);
        assert!(screen.contains("年报"), "{screen}");
        assert!(screen.contains("按计划 · 收入"), "{screen}");
        assert!(screen.contains("按计划 · 支出"), "{screen}");
        assert!(screen.contains("逐月走势"), "{screen}");
        assert!(screen.contains("占比"), "按计划支出应给出占比：\n{screen}");
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

        // 首屏是计划管理：运维项退到菜单最后，不占常驻版面
        let first = render_screen(&app, 110, 28);
        assert!(first.contains("运维 · 数据库"), "{first}");
        assert!(!first.contains("连接串"), "首屏不该铺连接表单：\n{first}");

        app.settings_section = SettingsSection::Connection;
        let screen = render_screen(&app, 110, 28);

        assert!(screen.contains("运维 · 数据库"), "{screen}");
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
        assert!(screen.contains("计划"), "{screen}");
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

    /// 120x40 下的整页快照 + 关键区块断言（也可人工核对排版）。
    #[test]
    fn wide_layout_keeps_its_panels() {
        let mut app = sample_app();
        for page in Page::ALL {
            app.page = page;
            let rendered = render_screen(&app, 120, 40);
            assert!(!rendered.trim().is_empty(), "{} 页不该是空白", page.title());
            println!("\n===== {} @120x40 =====\n{rendered}", page.title());
        }

        // 概览：本月一块条 / 日历 / 今日两块数字 / 最近动账
        app.page = Page::Overview;
        let screen = render_screen(&app, 120, 40);
        for needle in [
            "本月",
            "已花",
            "还剩",
            "s 同步今日",
            "今日额度",
            "支出日历",
            "最近动账",
        ] {
            assert!(screen.contains(needle), "概览缺少「{needle}」：\n{screen}");
        }

        // 设置：左菜单 + 右面板，日常计划在前、运维在后
        app.page = Page::Settings;
        let screen = render_screen(&app, 120, 40);
        for needle in ["▶ 分类", "运维 · 数据库"] {
            assert!(
                screen.contains(needle),
                "设置页缺少「{needle}」：\n{screen}"
            );
        }

        // 切到运维分区，连接面板和操作说明才出场
        app.settings_section = SettingsSection::Connection;
        let screen = render_screen(&app, 120, 40);
        for needle in ["连接串", "怎么选", "操作"] {
            assert!(
                screen.contains(needle),
                "设置页缺少「{needle}」：\n{screen}"
            );
        }
    }

    /// 空列表要给引导语，而不是一块白板。
    #[test]
    fn empty_states_guide_the_next_step() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.bills.clear();
        let screen = render_screen(&app, 120, 24);
        assert!(screen.contains("共 0 笔"), "{screen}");
        assert!(screen.contains("本月还没有账单"), "{screen}");
        assert!(screen.contains("按 a 记一笔"), "{screen}");

        // 设置 → 分类为空
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.categories.clear();
        let screen = render_screen(&app, 110, 24);
        assert!(screen.contains("还没有分类"), "{screen}");
        assert!(screen.contains("按 a 新增"), "{screen}");
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

    /// 新增账单：默认焦点在「金额」，打开就能直接输钱。
    #[test]
    fn new_bill_starts_on_the_amount_field() {
        let form = Form::new_bill(vec!["微信".to_string()], vec!["餐饮".to_string()], None);
        assert_eq!(form.fields[form.focus].label, "金额");
    }

    /// 选择列表是叠在表单上的，不能把表单顶掉；底部提示也不能被裁。
    #[test]
    fn picker_overlays_the_form_without_losing_it() {
        let mut app = sample_app();
        app.page = Page::Bills;
        let options = vec![
            "餐饮".to_string(),
            "  午餐".to_string(),
            "  晚餐".to_string(),
        ];
        app.modal = Some(Modal::Picker {
            form: Form::new_bill(vec!["微信".to_string()], options.clone(), None),
            field: 3,
            label: "分类",
            options,
            selected: 1,
        });

        let screen = render_screen(&app, 100, 30);
        assert!(screen.contains("新增账单"), "表单被列表顶掉了：\n{screen}");
        assert!(
            screen.contains("▶   午餐"),
            "列表没有高亮当前项：\n{screen}"
        );
        assert!(
            screen.contains("↑ ↓ 选择 · Enter 确认 · Esc 取消"),
            "底部提示被裁掉了：\n{screen}"
        );

        // 窄终端：列表挤不下时也不能把表单整个压没。
        let narrow = render_screen(&app, 80, 24);
        assert!(
            narrow.contains("╭─ 新增账单"),
            "窄屏下表单标题被切掉了：\n{narrow}"
        );
        assert!(
            narrow.contains("▶ 金额"),
            "窄屏下表单内容不见了：\n{narrow}"
        );
        assert!(
            narrow.contains("↑ ↓ 选择 · Enter 确认 · Esc 取消"),
            "窄屏下列表提示被裁掉了：\n{narrow}"
        );
    }

    /// 人工核对用：`cargo test print_layout_preview -- --nocapture`
    ///
    /// 按 120x40（规范里的基准尺寸）依次渲染五个页面，把每一页的 buffer 原样打印出来，
    /// 方便在终端里直接看排版；不 assert，平时不影响其它测试。
    #[test]
    fn print_layout_preview() {
        let mut app = sample_app();

        for page in Page::ALL {
            app.page = page;
            println!(
                "\n================ {} · 120x40 ================",
                page.title()
            );
            print!("{}\n", render_screen(&app, 120, 40));
            println!("\n{}\n", "─".repeat(60));
        }

        // 弹窗叠在概览页上，顺带看一眼圆角 + 标题 + 居中效果。
        app.page = Page::Overview;
        app.modal = Some(Modal::Notice {
            title: "提示".to_string(),
            message: "数据库连接已更新".to_string(),
        });
        println!("\n================ 提示弹窗 · 120x40 ================");
        print!("{}\n", render_screen(&app, 120, 40));
        println!();

        // 新增账单：表单本体 + 叠在上面的分类列表（列表不能把表单顶掉）。
        let options = vec![
            "餐饮".to_string(),
            "  午餐".to_string(),
            "  晚餐".to_string(),
            "交通".to_string(),
            "  地铁".to_string(),
        ];
        app.modal = Some(Modal::Form(Form::new_bill(
            vec!["微信".to_string()],
            options.clone(),
            None,
        )));
        println!("\n================ 新增账单 · 120x40 ================");
        print!("{}\n", render_screen(&app, 120, 40));
        println!();

        app.modal = Some(Modal::Picker {
            form: Form::new_bill(vec!["微信".to_string()], options.clone(), None),
            field: 3,
            label: "分类",
            options,
            selected: 1,
        });
        println!("\n================ 分类列表 · 120x40 ================");
        print!("{}\n", render_screen(&app, 120, 40));
        println!();
    }

    /// 界面快照基线：所有页面 + 所有弹窗，两种终端尺寸。
    ///
    /// 返回 (文件名, 渲染结果)。文件名只用 ASCII，避免 Windows 上的编码麻烦。
    fn golden_screens() -> Vec<(String, String)> {
        let mut app = sample_app();
        let mut screens = Vec::new();

        // 设置不在页签里，但它是首屏之一；报表两只面各拍一张，正好还是 6 张
        let shots: [(&str, Page, ReportTab); 6] = [
            ("page1", Page::Overview, ReportTab::Month),
            ("page2", Page::Plan, ReportTab::Month),
            ("page3", Page::Bills, ReportTab::Month),
            ("page4", Page::Reports, ReportTab::Month),
            ("page5", Page::Reports, ReportTab::Year),
            ("page6", Page::Settings, ReportTab::Month),
        ];
        for (name, page, tab) in shots {
            app.page = page;
            app.report_tab = tab;
            for (width, height) in [(120u16, 40u16), (80, 24)] {
                screens.push((
                    format!("{name}-{}x{}", width, height),
                    render_screen(&app, width, height),
                ));
            }
        }

        app.page = Page::Overview;
        app.modal = Some(Modal::Notice {
            title: "提示".to_string(),
            message: "数据库连接已更新".to_string(),
        });
        screens.push(("modal-notice-120x40".to_string(), render_screen(&app, 120, 40)));

        app.modal = Some(Modal::Confirm {
            title: "删除账单".to_string(),
            message: "确定删除这一笔？".to_string(),
            action: Action::DeleteBill(7),
        });
        screens.push(("modal-confirm-120x40".to_string(), render_screen(&app, 120, 40)));

        let options = vec![
            "餐饮".to_string(),
            "  午餐".to_string(),
            "  晚餐".to_string(),
            "交通".to_string(),
            "  地铁".to_string(),
        ];
        app.modal = Some(Modal::Form(Form::new_bill(
            vec!["微信".to_string()],
            options.clone(),
            None,
        )));
        screens.push(("modal-form-120x40".to_string(), render_screen(&app, 120, 40)));

        app.modal = Some(Modal::Picker {
            form: Form::new_bill(vec!["微信".to_string()], options.clone(), None),
            field: 3,
            label: "分类",
            options,
            selected: 1,
        });
        screens.push(("modal-picker-120x40".to_string(), render_screen(&app, 120, 40)));

        app.modal = Some(Modal::Help);
        screens.push(("modal-help-120x40".to_string(), render_screen(&app, 120, 40)));
        screens.push(("modal-help-80x24".to_string(), render_screen(&app, 80, 24)));

        screens
    }

    /// 把界面基线钉住：改排版时先跑它，确认差异都是想要的再 `BLESS=1` 重写。
    ///
    /// 更新基线：`BLESS=1 cargo test --lib golden_screens -- --nocapture`
    #[test]
    fn golden_screens_match() {
        // 放仓库根的 `golden/`：`.gitignore` 里有 `/tests`，放那儿基线不会被提交
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden");
        let bless = std::env::var_os("BLESS").is_some();

        if bless {
            std::fs::create_dir_all(&dir).expect("创建 baseline 目录");
        }

        let mut missing = Vec::new();
        let mut changed = Vec::new();

        for (name, screen) in golden_screens() {
            let path = dir.join(format!("{name}.txt"));

            if bless {
                std::fs::write(&path, &screen).expect("写入 baseline");
                continue;
            }

            match std::fs::read_to_string(&path) {
                Ok(expected) => {
                    if expected.trim_end() != screen.trim_end() {
                        changed.push(name);
                    }
                }
                Err(_) => missing.push(name),
            }
        }

        assert!(
            missing.is_empty(),
            "缺少界面基线：{}\n跑 BLESS=1 cargo test --lib golden_screens -- --nocapture 生成",
            missing.join(", ")
        );
        assert!(
            changed.is_empty(),
            "界面排版变了：{}\n确认是有意改动后跑 BLESS=1 更新基线，并把 diff 一起提交",
            changed.join(", ")
        );
    }
}
