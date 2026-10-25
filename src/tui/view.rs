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

use crate::bill::model::BillKind;
use crate::common::db;
use crate::common::table::{display_width, pad_right};
use crate::common::types::AmountType;
use crate::common::{money, time};
use crate::month::service::MonthReport;
use crate::plan::model::DayStatus;
use crate::tui::app::{App, Form, Modal, Page, SettingsFocus, SettingsSection};

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

/// 1 行顶栏：左「品牌 + 当前月 + 换月键」、中「胶囊页签」、右「连接状态」。
fn render_topbar(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    // 窄终端先砍掉右槽，再砍掉页签序号，保证左边永远先画出来
    let (left_w, tab_w, right_w) = if width >= 96 {
        (27usize, 44usize, width.saturating_sub(27 + 44))
    } else if width >= 76 {
        (27, 34, 0)
    } else {
        (width, 0, 0)
    };

    let cols = Layout::horizontal([
        Constraint::Length(left_w as u16),
        Constraint::Length(tab_w as u16),
        Constraint::Length(right_w as u16),
    ])
    .split(area);

    // 左：biller 2026-10  [ / ] 换月
    let left = Line::from(vec![
        Span::styled("biller", bold(ACCENT)),
        Span::raw(" "),
        Span::styled(time::format_month(app.month), bold(WARN)),
        Span::raw("  "),
        Span::styled("[ / ] 换月", muted()),
    ]);
    frame.render_widget(Paragraph::new(left), cols[0]);

    // 中：胶囊页签，当前页反色（ACCENT 底 + 黑字加粗）
    if tab_w > 0 {
        let numeric = tab_w >= 44;
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
        return ("未连接 · 按 5 配置".to_string(), WARN);
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
        Page::Bills => render_bills(frame, area, app),
        Page::Months => render_months(frame, area, app),
        Page::Years => render_years(frame, area, app),
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
            "按 5 进入「设置 → 数据库连接」，把连接串改成下面任意一种：",
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
// 设置页
// ---------------------------------------------------------------------------

/// 设置页：**左边一级菜单、右边二级内容**，底部固定「操作」面板。
fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::horizontal([Constraint::Length(18), Constraint::Min(30)]).split(area);

    render_settings_menu(frame, cols[0], app);

    let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(4)]).split(cols[1]);
    match app.settings_section {
        SettingsSection::Connection => render_connection_panel(frame, rows[0], app),
        SettingsSection::Books => render_books_panel(frame, rows[0], app),
        SettingsSection::Categories => render_categories_panel(frame, rows[0], app),
    }
    render_settings_actions(frame, rows[1], app);
}

/// 一级菜单：数据库连接 / 账本 / 分类。
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
            Line::from(Span::styled(
                "· 本地文件：零配置，单机自己用就够",
                muted(),
            )),
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

/// 账本分区：列表 + 位置提示。
fn render_books_panel(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.settings_index();
    // 焦点在左边菜单时，右侧列表只当预览看，不显示第二个光标
    let focused = app.settings_focus == SettingsFocus::Detail;
    let title = format!(
        "账本 · 共 {} 个{}",
        app.books.len(),
        if focused {
            position_hint(selected, app.books.len())
        } else {
            String::new()
        }
    );

    if app.books.is_empty() {
        render_placeholder(
            frame,
            area,
            block_with_title(true, title),
            "还没有账本",
            "按 a 新增一个，例如「微信」「支付宝」",
        );
        return;
    }

    let rows = app
        .books
        .iter()
        .enumerate()
        .map(|(index, book)| {
            let highlight_row = focused && index == selected;
            let id_cell = Cell::from(Span::styled(
                book.id.to_string(),
                if highlight_row { muted() } else { muted() },
            ));
            let name_cell = if highlight_row {
                Cell::from(Line::from(book.name.clone()))
            } else {
                Cell::from(Line::from(Span::styled(book.name.clone(), plain())))
            };
            Row::new(vec![id_cell, name_cell]).style(highlight(highlight_row))
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [Constraint::Length(6), Constraint::Min(12)])
        .header(Row::new(vec![header_cell("ID"), header_cell("账本")]).style(header_style()))
        .column_spacing(1)
        .block(block_with_title(focused, title));

    render_following_table(frame, area, table, focused.then_some(selected));
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
        SettingsSection::Books => {
            if app.settings_focus == SettingsFocus::Menu {
                actions.push(("Enter", "进列表"));
            } else {
                actions.push(("Enter", "改名"));
            }
            actions.push(("a", "新增账本"));
            actions.push(("e", "改名"));
            actions.push(("d", "删除"));
            "账本回答「钱从哪走」；删之前先把名下账单清掉"
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

/// 概览：银行同步（1 行）→ 当日检查器（3 行）→ 支出日历 / 本月概览（左右分栏）。
fn render_overview(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(3), // 银行同步
        Constraint::Length(5), // 当日检查器
        Constraint::Min(8),    // 日历 + 本月概览
    ])
    .split(area);

    render_sync_panel(frame, rows[0], app);

    let columns =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(rows[2]);
    render_day_checker(frame, rows[1], app);
    render_calendar(frame, columns[0], app);
    render_month_summary(frame, columns[1], app);
}

/// 银行同步：左两个胶囊按钮，右侧一句「怎么才算同步成功」。
fn render_sync_panel(frame: &mut Frame, area: Rect, app: &App) {
    let block = block_with_title(false, "银行同步");
    let inner = block.inner(area);

    if !app.pending_sync.is_empty() {
        let line = Line::from(vec![
            Span::styled("待核对 ", bold(WARN)),
            Span::styled(
                format!("{} 条", app.pending_sync.len()),
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · 按 Enter 继续逐条核对", muted()),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    let capsule = Style::default()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let left = Line::from(vec![
        Span::styled(" s 同步今日 ", capsule),
        Span::raw(" "),
        Span::styled(" w 同步本周 ", capsule),
    ]);
    let hint = "需先登录招商银行网银";
    let left_width = display_width(" s 同步今日 ") + 1 + display_width(" w 同步本周 ");
    let gap = (inner.width as usize)
        .saturating_sub(left_width)
        .saturating_sub(display_width(hint));
    let line = Line::from(vec![
        Span::styled(" s 同步今日 ", capsule),
        Span::raw(" "),
        Span::styled(" w 同步本周 ", capsule),
        Span::raw(" ".repeat(gap)),
        Span::styled(hint, muted()),
    ]);

    // 终端太窄时右对齐提示会被挤掉，宁可只留按钮
    let line = if gap >= 2 {
        line
    } else {
        left
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// 当日检查器：标题里带日期，正文三行——当天 / 累计 / 计划。
fn render_day_checker(frame: &mut Frame, area: Rect, app: &App) {
    let day = app.inspected_day();
    let title = format!(
        "{}-{:02}-{:02} {} ↑↓ 换日",
        app.month.year(),
        app.month.month(),
        day,
        weekday_name(app.month, day)
    );
    let block = block_with_title(true, title);
    let inner = block.inner(area);
    let width = inner.width as usize;

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

    let spent = progress.map(|progress| progress.spent).unwrap_or_else(|| {
        app.report
            .as_ref()
            .map(|report| report.collection.total_expense)
            .unwrap_or(0)
    });
    let limit = progress
        .map(|progress| progress.limit)
        .unwrap_or_else(|| app.plan.limit());
    let account = app
        .report
        .as_ref()
        .map(|report| report.collection.remaining())
        .unwrap_or(0);

    let day_color = if day_spent > day_allowance && day_allowance > 0 {
        BAD
    } else {
        TEXT
    };

    let mut lines = vec![
        stat_row(
            &[
                ("当日开销", money::format_amount(day_spent), bold(day_color)),
                ("当日额度", money::format_amount(day_allowance), plain()),
                (
                    "当日结余",
                    plus_amount(day_balance),
                    bold(if day_balance < 0 { BAD } else { GOOD }),
                ),
            ],
            width,
        ),
        stat_row(
            &[
                ("累计开销", money::format_amount(spent), bold(TEXT)),
                ("累计额度", money::format_amount(limit), plain()),
                ("账上余额", money::format_amount(account), plain()),
            ],
            width,
        ),
    ];

    // 第三行：计划本身 + 一个明确的修改入口
    let plan = &app.plan;
    let mut plan_spans: Vec<Span<'static>> = vec![Span::styled("本月计划 ", muted())];
    if plan.is_set() {
        plan_spans.push(Span::styled(
            format!(
                "预计 {} · 攒钱 {} · 已用 {}",
                money::format_amount(plan.expected),
                money::format_amount(plan.savings),
                percent(spent, limit),
            ),
            plain(),
        ));
    } else {
        plan_spans.push(Span::styled("还没设置，先设个本月上限", muted()));
    }
    plan_spans.push(Span::styled("  按 e 修改", muted()));
    lines.push(Line::from(plan_spans));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// 支出日历：7 列方格 + 底部两行图例 + 一行迷你进度条。
fn render_calendar(frame: &mut Frame, area: Rect, app: &App) {
    let limit = app
        .progress
        .as_ref()
        .map(|progress| progress.limit)
        .unwrap_or_else(|| app.plan.limit());
    let title = if app.plan.is_set() {
        format!("支出日历 · 上限 {}", money::format_amount(limit))
    } else {
        "支出日历".to_string()
    };
    let block = block_with_title(false, title);
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

    // 图例 1 行 + 提示 1 行 + 本月进度 1 行，固定占 3 行
    const CALENDAR_TAIL: usize = 3;
    let weeks = (leading as usize + days as usize).div_ceil(7);
    // 面板够高才用「日号 + 当天金额」两行一格；放不下时退回单行，保证尾部三行不被裁掉
    let two_rows = inner_height >= weeks * 2 + 1 + CALENDAR_TAIL;

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

    // 图例：两行刚好够表达绿 / 黄 / 红，再补一行本月进度
    let legend = vec![
        Line::from(vec![
            Span::styled("■", Style::default().fg(GOOD)),
            Span::styled(" ≤日额度  ", muted()),
            Span::styled("■", Style::default().fg(WARN)),
            Span::styled(" >日额度  ", muted()),
            Span::styled("■", Style::default().fg(BAD)),
            Span::styled(" >本月上限", muted()),
        ]),
        Line::from(Span::styled(
            "↑ ↓ 换日期 · [ ] 换月 · 按 m 回本月",
            muted(),
        )),
    ];
    let spent = app
        .progress
        .as_ref()
        .map(|progress| progress.spent)
        .unwrap_or(0);
    let (share_text, bar) = share(spent, limit.max(1), 10);
    let progress_line = Line::from(vec![
        Span::styled("本月已用 ", muted()),
        Span::styled(
            format!("{}/{}", money::format_amount(spent), money::format_amount(limit)),
            plain(),
        ),
        Span::raw(" "),
        Span::styled(bar, Style::default().fg(ACCENT)),
        Span::raw(" "),
        Span::styled(share_text, muted()),
    ]);

    // 周表头紧贴标题下沿往下排；图例 / 提示 / 进度条压到面板底部，
    // 中间空出来的高度夹在两者之间，读起来像「日历 + 页脚」而不是一堆散装文字
    let mut body = lines;
    let footer_len = legend.len() + 1;
    let slack = inner_height.saturating_sub(body.len() + footer_len);
    for _ in 0..slack {
        body.push(Line::from(""));
    }
    body.extend(legend);
    body.push(progress_line);

    frame.render_widget(Paragraph::new(body).block(block), area);
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

/// 本月概览：起始 / 收入 / 支出 → 分隔线 → 净额 / 结余 → 计划执行。
fn render_month_summary(frame: &mut Frame, area: Rect, app: &App) {
    let block = block_with_title(false, "本月概览");
    let inner = block.inner(area);
    let width = inner.width as usize;

    let Some(report) = app.report.as_ref() else {
        render_placeholder(
            frame,
            area,
            block,
            "正在汇总本月账单",
            "收入与支出就位后这里会出现三个数字",
        );
        return;
    };
    let collection = &report.collection;

    // 两列两行固定网格：同一行的数值对齐到同一条列边界，不会随内容长度左右抖
    let net = collection.net();
    let plan = &app.plan;
    let spent = app
        .progress
        .as_ref()
        .map(|progress| progress.spent)
        .unwrap_or(collection.total_expense);
    let limit = app
        .progress
        .as_ref()
        .map(|progress| progress.limit)
        .unwrap_or_else(|| plan.limit());
    let average = app
        .progress
        .as_ref()
        .map(|progress| progress.daily_average)
        .unwrap_or(0);
    let days = month_shape(app.month.year(), app.month.month()).1;
    let plan_percent = percent(spent, limit);
    let start = money::format_amount(collection.start_balance);
    let income = money::format_amount(collection.total_income);
    let expense = money::format_amount(collection.total_expense);
    let balance = money::format_amount(collection.remaining());
    let net_text = plus_amount(net);
    let net_style = bold(if net < 0 { BAD } else { GOOD });
    let divider = Line::from(Span::styled("─".repeat(width), Style::default().fg(BORDER)));

    let mut lines: Vec<Line<'static>> = if width < 44 {
        // 窄面板：单列排，金额统一右对齐到面板右边
        vec![
            stat_row(&[("起始金额", start, plain())], width),
            stat_row(&[("收入", income, bold(GOOD))], width),
            stat_row(&[("支出", expense, bold(BAD))], width),
            stat_row(&[("结余", balance, bold(TEXT))], width),
            divider,
            stat_row(&[("净额", net_text, net_style)], width),
            stat_row(&[("计划执行", plan_percent, bold(ACCENT))], width),
        ]
    } else {
        vec![
            stat_row(&[("起始金额", start, plain()), ("收入", income, bold(GOOD))], width),
            stat_row(&[("支出", expense, bold(BAD)), ("结余", balance, bold(TEXT))], width),
            divider,
            stat_row(
                &[
                    ("净额", net_text, net_style),
                    ("计划执行", plan_percent, bold(ACCENT)),
                ],
                width,
            ),
        ]
    };

    // 趋势行压到面板底部：上面是本月数字，最下面一行是日均 / 月底预计
    let inner_height = inner.height as usize;
    let slack = inner_height.saturating_sub(lines.len() + 1);
    for _ in 0..slack {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled("日均 ", muted()),
        Span::styled(money::format_amount(average), plain()),
        Span::styled(" · 月底预计 ", muted()),
        Span::styled(money::format_amount(average * days as i64), plain()),
    ]));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ---------------------------------------------------------------------------
// 账单
// ---------------------------------------------------------------------------

/// 账单：一整张表格，标题带月份与笔数，底部右对齐给出「第 i/n 条」。
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

    let selected = app.selected_index(Page::Bills);
    let rows = app
        .bills
        .iter()
        .enumerate()
        .map(|(index, bill)| {
            let chosen = index == selected;
            let date = bill.created_at.date_naive().to_string();
            let kind_style = match bill.kind {
                BillKind::Income => bold(GOOD),
                BillKind::Expense => bold(BAD),
            };
            let amount = format!(
                "{} {}",
                match bill.kind {
                    BillKind::Income => "+",
                    BillKind::Expense => "-",
                },
                money::format_amount(bill.amount)
            );
            let book = bill.book.name.clone();
            let category = bill.category.name.clone();
            let remark = bill.remark.clone().unwrap_or_default();
            let flag = if bill.excluded { "×" } else { "✓" };

            let cells = if chosen {
                vec![
                    Cell::from(date),
                    Cell::from(bill.kind.to_string()),
                    Cell::from(Line::from(amount).right_aligned()),
                    Cell::from(book),
                    Cell::from(category),
                    Cell::from(remark),
                    Cell::from(Line::from(flag).alignment(Alignment::Center)),
                ]
            } else {
                vec![
                    Cell::from(Span::styled(date, muted())),
                    Cell::from(Span::styled(bill.kind.to_string(), kind_style)),
                    Cell::from(
                        Line::from(Span::styled(
                            amount,
                            Style::default()
                                .fg(kind_style.fg.unwrap_or(TEXT))
                                .add_modifier(Modifier::BOLD),
                        ))
                        .right_aligned(),
                    ),
                    Cell::from(Span::styled(book, plain())),
                    Cell::from(Span::styled(category, plain())),
                    Cell::from(Span::styled(remark, muted())),
                    Cell::from(Line::from(Span::styled(flag, plain())).alignment(Alignment::Center)),
                ]
            };

            Row::new(cells).style(highlight(chosen))
        })
        .collect::<Vec<_>>();

    let block = block_with_title(true, title);
    let content = block.inner(area);

    // 「第 i/n 条」单独占面板内最后一行：不压底边框，也不和表格抢行数
    let table_area = Rect {
        height: content.height.saturating_sub(1),
        ..content
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(4),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(6),
            Constraint::Length(4),
        ],
    )
    .header(
        Row::new(vec![
            header_cell("日期"),
            header_cell("方向"),
            header_cell_at("金额", Alignment::Right),
            header_cell("账本"),
            header_cell("分类"),
            header_cell("备注"),
            header_cell_at("计列", Alignment::Center),
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
                    format!("第 {}/{} 条", selected + 1, total),
                    muted(),
                ))
                .right_aligned(),
            ),
            hint_area,
        );
    }
}

// ---------------------------------------------------------------------------
// 月报
// ---------------------------------------------------------------------------

/// 月报：左「汇总 + 按分类收入」、右「分类 + 账本排行 + 明细」。
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

/// 汇总卡片：2x2 网格 + 一条分隔线。
fn render_month_boxes(frame: &mut Frame, area: Rect, report: &MonthReport) {
    let collection = &report.collection;
    let block = block_with_title(false, "汇总");
    let inner = block.inner(area);
    let width = inner.width as usize;

    let lines = vec![
        stat_row(
            &[
                (
                    "起始金额",
                    money::format_amount(collection.start_balance),
                    plain(),
                ),
                (
                    "收入",
                    money::format_amount(collection.total_income),
                    bold(GOOD),
                ),
            ],
            width,
        ),
        stat_row(
            &[
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
            ],
            width,
        ),
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(BORDER),
        )),
    ];

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

/// 「支出 · 分类 + 账本」：排行 + 占比 + 条形图；选中行跟着明细走。
fn render_pair_table(frame: &mut Frame, area: Rect, app: &App, report: &MonthReport) {
    let collection = &report.collection;
    let pairs = collection.expense_pairs();
    let total = collection.total_expense;
    let selected = app.selected_index(Page::Months);
    let title = format!(
        "支出 · 分类 + 账本 · 合计 {}{}",
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
                        Line::from(Span::styled(
                            money::format_amount(*amount),
                            bold(rank),
                        ))
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
            header_cell("账本"),
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

/// 明细：当前选中「分类 + 账本」在本月的账单。
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
            "这个分类 + 账本本月没有账单",
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

/// 年报：顶部一行总账 → 中间按账本两张表 → 底部逐月走势。
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
        "按账本 · 收入",
        &collection.income_by_book,
        &report.book_names,
        collection.total_income,
        GOOD,
        false,
    );
    render_book_table(
        frame,
        books[1],
        "按账本 · 支出",
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

/// 按账本汇总：账本 / 金额 / 占比（支出再多一列分布条）。
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
    let mut entries: Vec<(u64, AmountType)> = values
        .iter()
        .map(|(id, amount)| (*id, *amount))
        .collect();
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
    let fixed = if with_bar { 8 + 12 + 7 + 3 } else { 8 + 12 + 7 + 2 };
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
        header_cell("账本"),
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

    let rows = (0..12usize)
        .map(|index| {
            let income = collection.income_by_month[index];
            let expense = collection.expense_by_month[index];
            let net = income - expense;
            let current =
                collection.year == app.month.year() && index + 1 == app.month.month() as usize;
            // 没有数据的月份整行弱化：数字走 MUTED、条形位只留一个 `·`，不再铺一片灰点
            let empty = income == 0 && expense == 0;
            let base = if current {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if empty {
                muted()
            } else {
                plain()
            };
            let net_style = if current {
                base
            } else if empty {
                muted()
            } else {
                Style::default().fg(if net < 0 { BAD } else { GOOD })
            };
            let mut cells = vec![
                Cell::from(Span::styled(format!("{} 月", index + 1), base)),
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
}

// ---------------------------------------------------------------------------
// 底部状态栏
// ---------------------------------------------------------------------------

/// 底栏：第 1 行状态，第 2 行上下文快捷键（按键说话，按页面换）。
fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    let (icon, message, style) = match app.status.as_ref() {
        Some(status) if status.is_error() => ("✖", status.text(), bold(BAD)),
        Some(status) => ("✔", status.text(), bold(GOOD)),
        None => ("◦", "就绪", muted()),
    };
    let right = format!(
        "{} · {}",
        app.page.title(),
        time::format_month(app.month)
    );
    let mut left: Vec<Span<'static>> = vec![
        Span::styled(format!("{icon} "), style),
        Span::styled(message.to_string(), style),
    ];
    let left_width: usize = left.iter().map(|span| display_width(span.content.as_ref())).sum();
    let gap = (rows[0].width as usize)
        .saturating_sub(left_width)
        .saturating_sub(display_width(&right));
    if gap >= 1 {
        left.push(Span::raw(" ".repeat(gap)));
    }
    left.push(Span::styled(right, muted()));
    frame.render_widget(Paragraph::new(Line::from(left)), rows[0]);

    // 第 2 行：上下文快捷键，最多 8 组，超了就砍尾部而不是缩成一团
    let keys = hotkeys_for(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (key, hint)) in keys.iter().take(8).enumerate() {
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
            Modal::Picker { .. } => vec![
                ("↑ ↓", "选择"),
                ("Enter", "确认"),
                ("Esc", "取消"),
            ],
            Modal::Confirm { .. } => vec![("y / Enter", "确认"), ("n / Esc", "取消")],
            Modal::Notice { .. } => vec![("任意键", "关闭")],
        };
    }

    match app.page {
        Page::Overview => {
            if !app.pending_sync.is_empty() {
                // 有没核完的流水时，Enter 是最该被看见的第一个键
                return vec![
                    ("Enter", "继续核对"),
                    ("↑ ↓", "换日"),
                    ("s", "同步今日"),
                    ("w", "同步本周"),
                    ("a", "新增账单"),
                    ("← →", "切页"),
                    ("q", "退出"),
                ];
            }
            vec![
                ("↑ ↓", "换日"),
                ("s", "同步今日"),
                ("w", "同步本周"),
                ("e", "本月设置"),
                ("a", "新增账单"),
                ("m", "回本月"),
                ("← →", "切页"),
                ("q", "退出"),
            ]
        }
        Page::Bills => vec![
            ("↑ ↓", "选择"),
            ("Enter", "编辑"),
            ("a", "新增账单"),
            ("d", "删除"),
            ("u", "撤销删除"),
            ("x", "计入 / 不计入"),
            ("← →", "切页"),
            ("q", "退出"),
        ],
        Page::Months => vec![
            ("↑ ↓", "选择"),
            ("Enter", "编辑账单"),
            ("a", "新增账单"),
            ("e", "本月设置"),
            ("m", "回本月"),
            ("[  ]", "换月"),
            ("← →", "切页"),
            ("q", "退出"),
        ],
        Page::Years => vec![
            ("[  ]", "换年"),
            ("e", "本月设置"),
            ("m", "回本月"),
            ("← →", "切页"),
            ("q", "退出"),
        ],
        Page::Settings => match app.settings_focus {
            // 连接分区没有列表可进，Enter 换成直接改连接，别让提示说谎
            SettingsFocus::Menu if app.settings_section == SettingsSection::Connection => vec![
                ("↑ ↓", "选分区"),
                ("e", "改连接"),
                ("i", "初始化"),
                ("t", "数据迁移"),
                ("← →", "切页"),
                ("q", "退出"),
            ],
            SettingsFocus::Menu => vec![
                ("↑ ↓", "选分区"),
                ("Enter", "进列表"),
                ("a", "新增"),
                ("← →", "切页"),
                ("q", "退出"),
            ],
            SettingsFocus::Detail => vec![
                ("↑ ↓", "选条目"),
                ("Enter", "编辑"),
                ("a", "新增"),
                ("d", "删除"),
                ("Esc", "回菜单"),
                ("← →", "切页"),
                ("q", "退出"),
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
        let pad = column
            .saturating_sub(display_width(label) + 1 + display_width(value));
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
fn percent(part: AmountType, whole: AmountType) -> String {
    if whole <= 0 {
        return "—".to_string();
    }
    format!("{:.1}%", (part as f64 / whole as f64) * 100.0)
}

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
    use crate::plan::model::{PlanProgress, SpendingPlan};
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
    fn connection_form_renders_both_modes() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Connection;

        // 远程模式：模式预选 PostgreSQL，PG 各项都在
        app.on_key(Key::Char('e'));
        let screen = render_screen(&app, 120, 44);
        assert!(screen.contains("模式"), "应有模式选择：\n{screen}");
        assert!(screen.contains("PostgreSQL"), "{screen}");
        assert!(screen.contains("文件路径"), "本地文件路径也要能填：\n{screen}");

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

        // 概览：同步按钮组 / 当日检查器 / 支出日历 / 本月数字 / 计划执行都在
        app.page = Page::Overview;
        let screen = render_screen(&app, 120, 40);
        for needle in [
            "银行同步",
            "s 同步今日",
            "w 同步本周",
            "当日额度",
            "本月计划",
            "支出日历",
            "本月概览",
            "计划执行",
        ] {
            assert!(screen.contains(needle), "概览缺少「{needle}」：\n{screen}");
        }

        // 设置：左菜单 + 右面板 + 操作提示都在同一个屏幕上
        app.page = Page::Settings;
        let screen = render_screen(&app, 120, 40);
        for needle in ["▶ 数据库连接", "连接串", "怎么选", "操作"] {
            assert!(screen.contains(needle), "设置页缺少「{needle}」：\n{screen}");
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

        // 设置 → 账本为空
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;
        app.books.clear();
        let screen = render_screen(&app, 110, 24);
        assert!(screen.contains("还没有账本"), "{screen}");
        assert!(screen.contains("按 a 新增一个"), "{screen}");
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
        assert!(screen.contains("▶   午餐"), "列表没有高亮当前项：\n{screen}");
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
        assert!(narrow.contains("▶ 金额"), "窄屏下表单内容不见了：\n{narrow}");
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
}
