//! 交互式 TUI 的状态机。
//!
//! 本模块只有纯逻辑：
//! 按键 → [`Action`]，以及 [`Action`] → 调用各领域 `service`。
//! 渲染与终端 IO 分别在 `view.rs` / `event.rs`，因此这里可以脱离终端测试。

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, Months, NaiveDate, Utc};

use crate::bill::model::{BillEntity, BillKind};
use crate::bill::service::{BillQuery, NewBill};
use crate::bill_book::model::BillBook;
use crate::category::model::Category;
use crate::category::service::{CategoryNode, descendants_of, subtree_of};
use crate::common::db::ConnectionSettings;
use crate::plan::model::{PlanMode, PlanProgress, SpendingPlan, days_in_month};
use crate::common::money;
use crate::common::types::{AmountType, BookId, CategoryId};
use crate::month::service::{MonthReport, YearReport};
use crate::tui::Services;
use crate::tui::input::Key;

/// 页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Bills,
    Months,
    Years,
    Settings,
}

impl Page {
    pub const ALL: [Page; 5] = [
        Page::Overview,
        Page::Bills,
        Page::Months,
        Page::Years,
        Page::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Overview => "概览",
            Page::Bills => "账单",
            Page::Months => "月报",
            Page::Years => "年报",
            Page::Settings => "设置",
        }
    }

    /// 该页是否支持上下选择行。
    pub fn is_list(self) -> bool {
        matches!(self, Page::Bills | Page::Months)
    }

    pub fn index(self) -> usize {
        Page::ALL.iter().position(|page| *page == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Page::ALL[(self.index() + 1) % Page::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = self.index();
        Page::ALL[(index + Page::ALL.len() - 1) % Page::ALL.len()]
    }
}

/// 设置页里的三个分区，`←` `→` 切换焦点。
///
/// 账本与分类以前是顶级页签，现在收进设置页：页签少两个，改数据的地方集中在一处。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Connection,
    Books,
    Categories,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 3] = [
        SettingsSection::Connection,
        SettingsSection::Books,
        SettingsSection::Categories,
    ];

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0)
    }

    /// 分区标题，同时也是面板边框上的字。
    pub fn title(self) -> &'static str {
        match self {
            SettingsSection::Connection => "数据库连接",
            SettingsSection::Books => "账本",
            SettingsSection::Categories => "分类",
        }
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = self.index();
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// 表单字段类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// 可选值列表，用 ←/→/空格 循环切换。
    ///
    /// 用 `Vec<String>` 而不是静态切片：父分类候选是运行时按分类树算出来的。
    Choice(Vec<String>),
}

/// 字段在键盘上的特殊行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRole {
    /// 普通文本字段。
    Plain,
    /// 日期字段：留空表示今天，空格填入上次使用过的日期。
    Date,
    /// 金额字段：内容可写成 `+ - * /` 算式，按 `=` 求值。
    Amount,
}

/// 表单里的一个字段。
#[derive(Debug, Clone)]
pub struct Field {
    pub label: &'static str,
    pub kind: FieldKind,
    pub value: String,
    pub hint: &'static str,
    pub role: FieldRole,
}

impl Field {
    fn text(label: &'static str, value: impl Into<String>, hint: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text,
            value: value.into(),
            hint,
            role: FieldRole::Plain,
        }
    }

    /// 日期字段：默认留空（留空即今天），按空格可填入上次使用过的日期。
    fn date(label: &'static str, hint: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text,
            value: String::new(),
            hint,
            role: FieldRole::Date,
        }
    }

    /// 金额字段：内容可写成算式（`+ - * /` 与括号），按 `=` 求值。
    fn amount(label: &'static str, hint: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text,
            value: String::new(),
            hint,
            role: FieldRole::Amount,
        }
    }

    fn choice(label: &'static str, options: Vec<String>, hint: &'static str) -> Self {
        let value = options.first().cloned().unwrap_or_default();
        Self {
            label,
            kind: FieldKind::Choice(options),
            value,
            hint,
            role: FieldRole::Plain,
        }
    }

    /// 带指定初值的选择字段（编辑表单预填用）。
    fn choice_with(
        label: &'static str,
        options: Vec<String>,
        initial: String,
        hint: &'static str,
    ) -> Self {
        let initial = initial.trim().to_string();
        let value = options
            .iter()
            .find(|option| **option == initial)
            .cloned()
            .unwrap_or_else(|| options.first().cloned().unwrap_or_default());
        Self {
            label,
            kind: FieldKind::Choice(options),
            value,
            hint,
            role: FieldRole::Plain,
        }
    }

    pub fn is_choice(&self) -> bool {
        matches!(&self.kind, FieldKind::Choice(_))
    }

    /// 选择框的候选项；文本框返回空切片。
    pub fn options(&self) -> &[String] {
        match &self.kind {
            FieldKind::Choice(options) => options,
            FieldKind::Text => &[],
        }
    }

    /// 在可选值之间循环。
    pub fn cycle(&mut self) {
        let FieldKind::Choice(options) = &self.kind else {
            return;
        };
        if options.is_empty() {
            return;
        }
        let position = options
            .iter()
            .position(|option| *option == self.value)
            .unwrap_or(0);
        self.value = options[(position + 1) % options.len()].clone();
    }

    pub fn push(&mut self, character: char) {
        if !self.is_choice() {
            self.value.push(character);
        }
    }

    pub fn backspace(&mut self) {
        if !self.is_choice() {
            self.value.pop();
        }
    }
}

/// 正在新增什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    NewBill,
    NewBook,
    NewCategory,
    /// 设置某月起始金额（概览页）。
    SetStartBalance,
    /// 数据库连接设置（设置页）。
    Connection,
    /// 数据迁移的目标库（设置页）。
    MigrateData,
}

/// 编辑目标：提交表单时据此决定调用哪个 service 的更新方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Bill(u64),
    Book(u64),
    Category(u64),
    /// 某月的起始金额（携带该月）。
    MonthBalance(NaiveDate),
}

/// 新增 / 编辑表单。
#[derive(Debug, Clone)]
pub struct Form {
    pub kind: FormKind,
    pub title: &'static str,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// `None` 表示新增，`Some` 表示编辑既有记录。
    pub target: Option<EditTarget>,
}

impl Form {
    /// 新增账单。
    ///
    /// `preferred_book` 是上次记账用过的账本（没有则传 `None`，回落到列表第一个）。
    /// `book_options` / `category_options` 由调用方从数据库里已读取的账本、分类填充
    /// （分类带层级缩进）；列表为空时该字段退化成文本框，直接输入会由 service 自动创建，
    /// 这样空库也能记第一笔账。
    ///
    /// 「日期」默认留空表示今天，需要在字段上按空格才填入上次用过的日期；
    /// 焦点默认落在「账本」——记账时它是最常用的选择项。
    pub fn new_bill(
        book_options: Vec<String>,
        category_options: Vec<String>,
        preferred_book: Option<&str>,
    ) -> Self {
        Self {
            kind: FormKind::NewBill,
            title: "新增账单",
            fields: vec![
                Field::choice(
                    "方向",
                    vec!["支出".to_string(), "收入".to_string()],
                    "←/→ 或空格切换",
                ),
                Field::amount("金额", "可写算式（+ - * / 与括号），按 = 求值"),
                picker(
                    "账本",
                    book_options,
                    preferred_book,
                    "Enter 展开选择；新增请到「账本」页",
                ),
                picker(
                    "分类",
                    category_options,
                    None,
                    "Enter 展开选择；新增请到「分类」页",
                ),
                Field::date("日期", "留空即今天；按空格填入上次使用的时间"),
                Field::text("备注", "", "可留空"),
            ],
            focus: 2,
            target: None,
        }
    }

    pub fn new_book() -> Self {
        Self {
            kind: FormKind::NewBook,
            title: "新增账本",
            fields: vec![Field::text("名称", "", "例如 微信")],
            focus: 0,
            target: None,
        }
    }

    /// 新增分类；`parent_options` 是带层级缩进的父分类候选（首项为「(顶层)」）。
    pub fn new_category(parent_options: Vec<String>) -> Self {
        Self {
            kind: FormKind::NewCategory,
            title: "新增分类",
            fields: vec![
                Field::text("名称", "", "例如 餐饮"),
                Field::choice_with(
                    "父分类",
                    parent_options,
                    "(顶层)".to_string(),
                    "Enter 展开选择；缩进表示层级",
                ),
            ],
            focus: 0,
            target: None,
        }
    }

    /// 编辑账单：各字段预填为当前值。
    pub fn edit_bill(
        bill: &BillEntity,
        book_options: Vec<String>,
        category_options: Vec<String>,
    ) -> Self {
        let mut form = Self::new_bill(book_options, category_options, None);
        form.title = "编辑账单";
        form.target = Some(EditTarget::Bill(bill.id));
        form.focus = 1;
        form.fields[0].value = match bill.kind {
            BillKind::Income => "收入".to_string(),
            BillKind::Expense => "支出".to_string(),
        };
        form.fields[1].value = money::format_amount(bill.amount);
        set_field_value(&mut form.fields[2], &bill.book.name);
        set_field_value(&mut form.fields[3], &bill.category.name);
        form.fields[4].value = crate::common::time::format_datetime(bill.created_at);
        form.fields[5].value = bill.remark.clone().unwrap_or_default();
        // 账单详情里也能改「计入日历」
        form.fields.push(Field::choice_with(
            "计入日历",
            vec!["计入".to_string(), "不计入".to_string()],
            if bill.excluded {
                "不计入".to_string()
            } else {
                "计入".to_string()
            },
            "只影响支出日历与开销计划；月报/年报始终全额计入",
        ));
        form
    }

    /// 编辑账本：预填当前名称。
    pub fn edit_book(book: &BillBook) -> Self {
        let mut form = Self::new_book();
        form.title = "编辑账本";
        form.target = Some(EditTarget::Book(book.id));
        form.fields[0].value = book.name.clone();
        form
    }

    /// 编辑分类：预填名称与当前父分类。
    pub fn edit_category(
        category: &Category,
        parent_options: Vec<String>,
        current_parent_label: &str,
    ) -> Self {
        let mut form = Self::new_category(parent_options);
        form.title = "编辑分类";
        form.target = Some(EditTarget::Category(category.id));
        form.fields[0].value = category.name.clone();
        form.fields[1].value = current_parent_label.to_string();
        form
    }

    /// 本月设置（概览页按 `e`）：起始金额与开销计划一起改。
    ///
    /// 计划只有两个数：**上限开销 = 预计 − 攒钱**，不单独存。
    pub fn month_settings(month: NaiveDate, current: AmountType, plan: SpendingPlan) -> Self {
        Self {
            kind: FormKind::SetStartBalance,
            title: "本月设置（起始金额 + 开销计划）",
            fields: vec![
                Field::text(
                    "起始金额",
                    money::format_amount(current),
                    "元，例如 1000 或 1000.50",
                ),
                Field::text(
                    "预计金额",
                    money::format_amount(plan.expected),
                    "本月预计到手多少（元）",
                ),
                Field::text(
                    "攒钱金额",
                    money::format_amount(plan.savings),
                    "打算攒下多少；上限开销 = 预计 − 攒钱",
                ),
                Field::choice_with(
                    "超支判断",
                    vec![
                        PlanMode::Cumulative.label().to_string(),
                        PlanMode::DailyOnly.label().to_string(),
                    ],
                    plan.mode.label().to_string(),
                    "累计标红：超了之后一路红；只看当天：每天独立判断",
                ),
            ],
            focus: 0,
            target: Some(EditTarget::MonthBalance(month)),
        }
    }

    /// 数据库连接设置（设置页按 `e`）。
    pub fn connection(settings: &ConnectionSettings) -> Self {
        Self {
            kind: FormKind::Connection,
            title: "数据库连接",
            fields: vec![
                Field::text("主机", settings.host.clone(), "IP 或主机名"),
                Field::text("端口", settings.port.to_string(), "默认 5432"),
                Field::text("用户名", settings.user.clone(), "PostgreSQL 用户"),
                Field::text("密码", settings.password.clone(), "会以明文写进 .env"),
                Field::text("数据库", settings.database.clone(), "不存在时会自动创建"),
            ],
            focus: 0,
            target: None,
        }
    }

    /// 数据迁移的目标库（设置页按 `t`）。源库固定是「当前连接」。
    pub fn migrate_target(settings: &ConnectionSettings) -> Self {        let mut form = Self::connection(settings);
        form.kind = FormKind::MigrateData;
        form.title = "数据迁移（源库 = 当前连接）";

        // 字段顺序：主机 / 端口 / 用户名 / 密码 / 数据库
        if let Some(field) = form.fields.get_mut(4) {
            field.hint = "目标库：会被清空后写入源库的全部数据";
        }

        form
    }

    pub fn value(&self, label: &str) -> &str {
        self.fields
            .iter()
            .find(|field| field.label == label)
            .map(|field| field.value.as_str())
            .unwrap_or_default()
            .trim()
    }

    pub fn current(&self) -> Option<&Field> {
        self.fields.get(self.focus)
    }

    pub fn current_is_choice(&self) -> bool {
        self.current().is_some_and(Field::is_choice)
    }

    pub fn next_field(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + 1) % self.fields.len();
        }
    }

    pub fn previous_field(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
        }
    }

    pub fn cycle_current(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cycle();
        }
    }

    pub fn push(&mut self, character: char) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.push(character);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.backspace();
        }
    }
}

/// 弹窗。
#[derive(Debug, Clone)]
pub enum Modal {
    Form(Form),
    /// 展开的选择列表：覆盖在表单之上，回车确认后回填到 `field`。
    Picker {
        form: Form,
        field: usize,
        label: &'static str,
        options: Vec<String>,
        selected: usize,
    },
    Confirm {
        title: String,
        message: String,
        action: Action,
    },
    Notice {
        title: String,
        message: String,
    },
}

/// 状态栏消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Info(String),
    Error(String),
}

impl Status {
    pub fn text(&self) -> &str {
        match self {
            Status::Info(text) | Status::Error(text) => text,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Status::Error(_))
    }
}

/// 状态机输出的动作；由外壳负责执行（可能访问数据库）。
#[derive(Debug, Clone)]
pub enum Action {
    None,
    Refresh,
    ShiftMonth(i32),
    /// 年报页换年。
    ShiftYear(i32),
    SubmitForm(Form),
    DeleteBill(u64),
    DeleteBook(u64),
    /// 删除分类；`recursive` 为真时连同子孙一起删（终端会先提示）。
    DeleteCategory {
        id: u64,
        recursive: bool,
    },
    /// 保存连接设置并尝试立即切换（连接池由外壳重建）。
    SaveConnection(ConnectionSettings),
    /// 建库 + 建表（初始化），由外壳执行。
    InitDatabase,
    /// 把当前库的全部数据迁移到目标库，由外壳执行。
    MigrateData(ConnectionSettings),
    /// 撤销最近一次删除。
    UndoDelete,
    /// 切换一条账单是否纳入统计（账单页 `x`）。
    ToggleExcluded(u64),
}

/// 最近一次删除的内容，供 `u` 撤销。
///
/// 只保留**最后一笔**：撤销是为了救手滑，不做回收站。
#[derive(Debug, Clone)]
pub enum DeletedRecord {
    Bill(Box<BillEntity>),
    Book {
        name: String,
    },
    /// 一整棵被删掉的子树。顺序是「父先子后」，照着塞回去即可。
    Categories(Vec<DeletedCategory>),
}

/// 待恢复的一个分类：靠**名字**重新挂回父分类（id 已经没了）。
#[derive(Debug, Clone)]
pub struct DeletedCategory {
    pub name: String,
    pub parent_name: Option<String>,
}

/// 整个 TUI 的状态。
pub struct App {
    pub page: Page,
    pub month: NaiveDate,
    /// 年报页查看的年份。
    pub year: i32,
    /// 当前生效的连接参数（设置页展示与编辑的就是它）。
    pub connection: ConnectionSettings,
    /// 当前生效的完整连接串。
    pub connection_url: String,
    /// 最近一次删除，按 `u` 撤销。
    pub last_deleted: Option<DeletedRecord>,
    /// 设置页当前焦点所在的分区。
    pub settings_section: SettingsSection,
    /// 三个分区各自的选中行。
    settings_selected: [usize; SettingsSection::ALL.len()],
    /// 开销计划（预计金额 / 攒钱金额），全局生效。
    pub plan: SpendingPlan,
    /// 当月计划的执行情况与日历配色，在 `reload` 时算一次。
    pub progress: Option<PlanProgress>,
    /// 概览页正在查看的那一天（1..=当月天数），`←` `→` 移动。
    pub selected_day: u32,
    /// 上次记账用过的日期（跨会话记忆）；`None` 表示还没记过。
    /// 新增表单里留空即今天，按空格才把它填进去。
    pub last_entry_date: Option<NaiveDate>,
    /// 新增账单时默认选中的账本：上次记账用过的账本名（跨会话记忆）。
    pub last_entry_book: Option<String>,
    /// 每页各自记住选中行。
    pub selected: [usize; Page::ALL.len()],
    pub books: Vec<BillBook>,
    /// 分类按层级展开，`depth` 用于渲染缩进与父分类候选。
    pub categories: Vec<CategoryNode>,
    pub bills: Vec<BillEntity>,
    pub report: Option<MonthReport>,
    pub year_report: Option<YearReport>,
    pub modal: Option<Modal>,
    pub status: Option<Status>,
    pub should_quit: bool,
}

impl App {
    pub fn new(month: NaiveDate, connection_url: &str) -> Self {
        let today = Self::today();

        Self {
            page: Page::Overview,
            month,
            year: month.year(),
            connection: ConnectionSettings::from_url(connection_url),
            connection_url: connection_url.to_string(),
            last_deleted: None,
            settings_section: SettingsSection::Connection,
            settings_selected: [0; SettingsSection::ALL.len()],
            plan: SpendingPlan::default(),
            progress: None,
            // 看本月就默认从今天开始看，看历史月份从 1 号开始
            selected_day: if today.year() == month.year() && today.month() == month.month() {
                today.day()
            } else {
                1
            },
            last_entry_date: None,
            last_entry_book: None,
            selected: [0; Page::ALL.len()],
            books: Vec::new(),
            categories: Vec::new(),
            bills: Vec::new(),
            report: None,
            year_report: None,
            modal: None,
            status: None,
            should_quit: false,
        }
    }

    /// 今天（UTC 日期，与账单时间同一口径）。
    pub fn today() -> NaiveDate {
        Utc::now().date_naive()
    }

    /// 今日指标：`(截至今天的剩余, 今天的开销)`。
    ///
    /// 剩余 = 本月起始金额 + 截至今日的收入 − 截至今日的支出，
    /// 也就是「到今天为止手上还剩多少」。
    ///
    /// 只在查看**本月**时才有数据——`bills` 里只装了当前查看月份的账单，
    /// 翻到别的月份时今天的账单根本不在手上，返回 `None` 让界面说明情况。
    pub fn today_totals(&self) -> Option<(AmountType, AmountType)> {
        if !self.viewing_current_month() {
            return None;
        }

        let today = Self::today();
        let report = self.report.as_ref()?;
        let mut income = 0;
        let mut expense = 0;
        let mut today_expense = 0;

        for bill in &self.bills {
            let date = bill.created_at.date_naive();
            // 记到未来日期的账单不算「截至今日」
            if date > today {
                continue;
            }

            match bill.kind {
                BillKind::Income => income += bill.amount,
                BillKind::Expense => {
                    expense += bill.amount;
                    if date == today {
                        today_expense += bill.amount;
                    }
                }
            }
        }

        Some((
            report.collection.start_balance + income - expense,
            today_expense,
        ))
    }

    /// 今天的支出排行：按「分类 + 账本」成对，只在查看本月时有数据。
    ///
    /// 概览页用它，和月报（整月构成）分工——**概览看今天，月报看本月**，两页不重复。
    pub fn today_expense_pairs(&self) -> Vec<(CategoryId, BookId, AmountType)> {
        if !self.viewing_current_month() {
            return Vec::new();
        }

        let today = Self::today();
        let mut totals: HashMap<(CategoryId, BookId), AmountType> = HashMap::new();

        for bill in &self.bills {
            if bill.kind != BillKind::Expense || bill.created_at.date_naive() != today {
                continue;
            }
            *totals.entry((bill.category.id, bill.book.id)).or_default() += bill.amount;
        }

        crate::month::model::rank_pairs(totals)
    }

    /// 当前查看的是不是本月——今天的账单只装在本月的数据里。
    ///
    /// 翻到别的月份时 `bills` 全是那个月的账，今天的账根本不在手上。
    pub fn viewing_current_month(&self) -> bool {
        let today = Self::today();
        self.report.as_ref().is_some_and(|report| {
            let month = report.collection.month;
            month.year() == today.year() && month.month() == today.month()
        })
    }

    /// 当前页可供选择的行数。
    pub fn row_count(&self, page: Page) -> usize {
        match page {
            Page::Bills => self.bills.len(),
            Page::Months => self.month_expense_pairs().len(),
            Page::Overview | Page::Years | Page::Settings => 0,
        }
    }

    /// 月报页的支出排行：按「分类 + 账本」成对列出，金额从多到少。
    ///
    /// 只按分类汇总会把「钱从哪本账走」抹平——日常开销分散在多本账上时，
    /// 单看分类看不出某本账才是大头，成对比较才真实。
    pub fn month_expense_pairs(&self) -> Vec<(CategoryId, BookId, AmountType)> {
        self.report
            .as_ref()
            .map(|report| report.collection.expense_pairs())
            .unwrap_or_default()
    }

    /// 月报页右栏：当前选中的「分类 + 账本」在本月的账单。
    pub fn month_selected_bills(&self) -> Vec<&BillEntity> {
        let Some((category_id, book_id, _)) = self
            .month_expense_pairs()
            .get(self.selected_index(Page::Months))
            .copied()
        else {
            return Vec::new();
        };

        self.bills
            .iter()
            .filter(|bill| bill.category.id == category_id && bill.book.id == book_id)
            .collect()
    }

    /// 月报页当前选中的那一行描述，形如「餐饮 @ 微信」。
    pub fn month_selected_label(&self) -> Option<String> {
        let (category_id, book_id, _) = self
            .month_expense_pairs()
            .get(self.selected_index(Page::Months))
            .copied()?;

        let report = self.report.as_ref()?;
        let category = report
            .category_names
            .get(&category_id)
            .cloned()
            .unwrap_or_else(|| format!("(已删除 id={category_id})"));
        let book = report
            .book_names
            .get(&book_id)
            .cloned()
            .unwrap_or_else(|| format!("(已删除 id={book_id})"));

        Some(format!("{category} @ {book}"))
    }

    /// 概览页正在查看的那一天（按当月天数钳制）。
    pub fn inspected_day(&self) -> u32 {
        let days = days_in_month(self.month).max(1);
        self.selected_day.clamp(1, days)
    }

    /// 概览页用 `↑` `↓` 在当月内移动查看的那一天（到边界停住，不跨月）。
    fn move_inspected_day(&mut self, delta: i32) {
        let days = days_in_month(self.month).max(1) as i32;
        let next = (self.inspected_day() as i32 + delta).clamp(1, days);
        self.selected_day = next as u32;
    }

    /// 截至查看日（含）的**现金结余**：本月起始金额 + 截至该日收入 − 截至该日支出。
    ///
    /// 与「到该日剩余」不是一回事：那个是预算口径（上限 − 累计支出），
    /// 这个是账上还剩多少，**全额计入，不看 `excluded`**。
    pub fn balance_through_inspected_day(&self) -> Option<AmountType> {
        let report = self.report.as_ref()?;
        let day = self.inspected_day();

        let mut income = 0;
        let mut expense = 0;
        for bill in &self.bills {
            if bill.created_at.date_naive().day() > day {
                continue;
            }
            match bill.kind {
                BillKind::Income => income += bill.amount,
                BillKind::Expense => expense += bill.amount,
            }
        }

        Some(report.collection.start_balance + income - expense)
    }

    /// 设置页当前分区的可选行数（连接分区没有列表）。
    pub fn settings_row_count(&self) -> usize {
        match self.settings_section {
            SettingsSection::Connection => 0,
            SettingsSection::Books => self.books.len(),
            SettingsSection::Categories => self.categories.len(),
        }
    }

    /// 设置页当前分区的选中行（已钳制）。
    pub fn settings_index(&self) -> usize {
        let count = self.settings_row_count();
        if count == 0 {
            0
        } else {
            self.settings_selected[self.settings_section.index()].min(count - 1)
        }
    }

    /// 设置页当前选中的账本。
    fn selected_book(&self) -> Option<&BillBook> {
        self.books.get(self.settings_index())
    }

    /// 设置页当前选中的分类节点。
    fn selected_category(&self) -> Option<&CategoryNode> {
        self.categories.get(self.settings_index())
    }

    /// 上下移动只作用于设置页当前分区。
    fn move_settings_selection(&mut self, delta: i32) {
        let count = self.settings_row_count();
        if count == 0 {
            return;
        }
        let current = self.settings_index() as i32;
        let next = (current + delta).rem_euclid(count as i32);
        self.settings_selected[self.settings_section.index()] = next as usize;
    }

    /// 把当前上下文（页面，或设置页当前分区）的选中行跳到指定位置。
    fn jump_selection(&mut self, index: usize) {
        if self.page == Page::Settings {
            self.settings_selected[self.settings_section.index()] = index;
        } else {
            self.selected[self.page.index()] = index;
        }
    }

    /// 当前上下文的可选行数。
    fn current_row_count(&self) -> usize {
        if self.page == Page::Settings {
            self.settings_row_count()
        } else {
            self.row_count(self.page)
        }
    }

    pub fn selected_index(&self, page: Page) -> usize {
        let count = self.row_count(page);
        if count == 0 {
            0
        } else {
            self.selected[page.index()].min(count - 1)
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.page == Page::Settings {
            self.move_settings_selection(delta);
            return;
        }

        let page = self.page;
        let count = self.row_count(page);
        if count == 0 {
            return;
        }
        let current = self.selected_index(page) as i32;
        let next = (current + delta).rem_euclid(count as i32);
        self.selected[page.index()] = next as usize;
    }

    fn clamp_selection(&mut self) {
        for page in Page::ALL {
            let count = self.row_count(page);
            if count == 0 {
                self.selected[page.index()] = 0;
            } else {
                self.selected[page.index()] = self.selected[page.index()].min(count - 1);
            }
        }
    }

    pub fn set_info(&mut self, message: impl Into<String>) {
        self.status = Some(Status::Info(message.into()));
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.status = Some(Status::Error(message.into()));
    }

    /// 扁平分类列表（分类树算法需要）。
    fn flat_categories(&self) -> Vec<Category> {
        self.categories
            .iter()
            .map(|node| node.category.clone())
            .collect()
    }

    /// 父分类候选：首项「(顶层)」，其余按树层级缩进。
    ///
    /// `exclude` 用于编辑时排除自身与全体子孙，从源头上避免把分类挂到自己下面。
    fn parent_options(&self, exclude: Option<CategoryId>) -> Vec<String> {
        let excluded: HashSet<CategoryId> = match exclude {
            Some(id) => {
                let mut set = descendants_of(&self.flat_categories(), id);
                set.insert(id);
                set
            }
            None => HashSet::new(),
        };

        let mut options = vec!["(顶层)".to_string()];
        for node in &self.categories {
            if excluded.contains(&node.category.id) {
                continue;
            }
            options.push(format!(
                "{}{}",
                "  ".repeat(node.depth),
                node.category.name
            ));
        }
        options
    }

    /// 某分类在候选项里的显示标签（带缩进）。
    fn parent_label(&self, id: CategoryId) -> String {
        self.categories
            .iter()
            .find(|node| node.category.id == id)
            .map(|node| format!("{}{}", "  ".repeat(node.depth), node.category.name))
            .unwrap_or_else(|| "(顶层)".to_string())
    }

    /// 已有账本名称，供账单表单选择。
    fn book_names(&self) -> Vec<String> {
        self.books.iter().map(|book| book.name.clone()).collect()
    }

    /// 已有分类名称（带层级缩进），供账单表单选择。
    fn category_names(&self) -> Vec<String> {
        self.categories
            .iter()
            .map(|node| format!("{}{}", "  ".repeat(node.depth), node.category.name))
            .collect()
    }

    fn open_default_form(&mut self) {
        let form = match self.page {
            // 设置页按 `a` 新建的，是当前分区里的东西
            Page::Settings => match self.settings_section {
                SettingsSection::Connection => Form::connection(&self.connection),
                SettingsSection::Books => Form::new_book(),
                SettingsSection::Categories => Form::new_category(self.parent_options(None)),
            },
            // 概览/账单/月报/年报都认为你要记账
            Page::Overview | Page::Bills | Page::Months | Page::Years => Form::new_bill(
                self.book_names(),
                self.category_names(),
                self.last_entry_book.as_deref(),
            ),
        };
        self.modal = Some(Modal::Form(form));
    }

    /// 设置页按 `t`：打开「数据迁移」表单（目标库默认预填当前连接）。
    fn open_migrate_form(&mut self) {
        self.modal = Some(Modal::Form(Form::migrate_target(&self.connection)));
    }

    fn open_edit_form(&mut self) {
        let form = match self.page {
            // 概览页的「可编辑内容」是起始金额 + 开销计划；设置页就是连接参数
            Page::Overview => {
                let current = self
                    .report
                    .as_ref()
                    .map(|report| report.collection.start_balance)
                    .unwrap_or(0);
                Some(Form::month_settings(self.month, current, self.plan))
            }
            // 设置页按 `e`：改的是当前分区里的东西
            Page::Settings => match self.settings_section {
                SettingsSection::Connection => Some(Form::connection(&self.connection)),
                SettingsSection::Books => self.selected_book().map(Form::edit_book),
                SettingsSection::Categories => self.selected_category().and_then(|node| {
                    let options = self.parent_options(Some(node.category.id));
                    let label = match node.category.parent_id {
                        Some(parent_id) => self.parent_label(parent_id),
                        None => "(顶层)".to_string(),
                    };
                    Some(Form::edit_category(&node.category, options, &label))
                }),
            },
            Page::Bills => self
                .bills
                .get(self.selected_index(Page::Bills))
                .map(|bill| Form::edit_bill(bill, self.book_names(), self.category_names())),
            Page::Months | Page::Years => None,
        };

        match form {
            Some(form) => self.modal = Some(Modal::Form(form)),
            None => self.set_info("当前页没有可编辑的内容"),
        }
    }

    /// 按 id 从当前账单里取一份快照，供撤销恢复。
    pub fn bill_record(&self, id: u64) -> Option<DeletedRecord> {
        self.bills
            .iter()
            .find(|bill| bill.id == id)
            .cloned()
            .map(|bill| DeletedRecord::Bill(Box::new(bill)))
    }

    /// 把「刚被删掉的那串分类」整理成可恢复的记录。
    ///
    /// 删除顺序是「子先父后」，倒过来正好是恢复需要的「父先子后」；
    /// 父分类名字要从**删除前**的树里查，所以调用时机是删完但还没 `reload` 的时候。
    pub fn category_record(&self, removed: &[crate::category::model::Category]) -> DeletedRecord {
        let entries = removed
            .iter()
            .rev()
            .map(|category| DeletedCategory {
                name: category.name.clone(),
                parent_name: category
                    .parent_id
                    .and_then(|parent| self.category_name(parent)),
            })
            .collect();

        DeletedRecord::Categories(entries)
    }

    /// 按 id 取分类名（撤销时用来把子分类挂回父分类）。
    fn category_name(&self, id: u64) -> Option<String> {
        self.categories
            .iter()
            .find(|node| node.category.id == id)
            .map(|node| node.category.name.clone())
    }

    /// 删除分类的确认文案与动作（含「会连带删掉哪些子分类」）。
    fn category_delete_request(&self) -> Option<(String, Action)> {
        let node = self.selected_category()?.clone();

        // 先把「会连带删掉哪些子分类」讲清楚，再让用户确认
        let subtree = subtree_of(&self.flat_categories(), node.category.id);
        let children: Vec<&Category> = subtree
            .iter()
            .filter(|item| item.id != node.category.id)
            .collect();

        let message = if children.is_empty() {
            format!(
                "确定要删除分类「{}」吗？此操作不可撤销。",
                node.category.name
            )
        } else {
            let names = children
                .iter()
                .map(|item| item.name.clone())
                .collect::<Vec<_>>()
                .join("、");
            format!(
                "分类「{}」下有 {} 个子分类（{}），将一并删除。确定吗？",
                node.category.name,
                children.len(),
                names
            )
        };

        Some((
            message,
            Action::DeleteCategory {
                id: node.category.id,
                recursive: !children.is_empty(),
            },
        ))
    }

    fn request_delete(&mut self) {
        let request = match self.page {
            Page::Bills => self
                .bills
                .get(self.selected_index(Page::Bills))
                .map(|bill| {
                    (
                        format!(
                            "确定要删除账单 #{}（{} {} 元）吗？此操作不可撤销。",
                            bill.id,
                            bill.kind,
                            money::format_amount(bill.amount)
                        ),
                        Action::DeleteBill(bill.id),
                    )
                }),
            Page::Settings => match self.settings_section {
                // 连接分区没有可删的东西
                SettingsSection::Connection => None,
                SettingsSection::Books => self.selected_book().map(|book| {
                    (
                        format!("确定要删除账本「{}」吗？此操作不可撤销。", book.name),
                        Action::DeleteBook(book.id),
                    )
                }),
                SettingsSection::Categories => self.category_delete_request(),
            },
            // 概览/月报/年报没有可删的东西（设置页已经在上面按分区处理了）
            Page::Overview | Page::Months | Page::Years => None,
        };

        match request {
            Some((message, action)) => {
                self.modal = Some(Modal::Confirm {
                    title: "确认删除".to_string(),
                    message,
                    action,
                });
            }
            None => self.set_info("当前页没有可删除的内容"),
        }
    }

    /// `[` / `]` 的含义随页面变化：年报页换年，其余页换月。
    fn shift_period(&self, delta: i32) -> Action {
        if self.page == Page::Years {
            Action::ShiftYear(delta)
        } else {
            Action::ShiftMonth(delta)
        }
    }

    /// 处理一次按键，返回外壳需要执行的动作。
    pub fn on_key(&mut self, key: Key) -> Action {
        if self.modal.is_some() {
            return self.on_key_modal(key);
        }

        // Ctrl+C / q 退出
        if key == Key::Ctrl('c') || key == Key::Char('q') {
            self.should_quit = true;
            return Action::None;
        }

        match key {
            Key::Char('1'..='7') => {
                if let Key::Char(digit) = key {
                    let index = digit as usize - '1' as usize;
                    if let Some(page) = Page::ALL.get(index) {
                        self.page = *page;
                        return Action::Refresh;
                    }
                }
                Action::None
            }
            Key::Tab => {
                self.page = self.page.next();
                Action::Refresh
            }
            Key::BackTab => {
                self.page = self.page.previous();
                Action::Refresh
            }
            Key::Up => {
                if self.page == Page::Overview {
                    // 概览页用 ↑↓ 换查看的日期
                    self.move_inspected_day(-1);
                    Action::Refresh
                } else {
                    self.move_selection(-1);
                    Action::None
                }
            }
            Key::Down => {
                if self.page == Page::Overview {
                    self.move_inspected_day(1);
                    Action::Refresh
                } else {
                    self.move_selection(1);
                    Action::None
                }
            }
            Key::PageUp => {
                self.move_selection(-10);
                Action::None
            }
            Key::PageDown => {
                self.move_selection(10);
                Action::None
            }
            Key::Home => {
                self.jump_selection(0);
                Action::None
            }
            Key::End => {
                let count = self.current_row_count();
                self.jump_selection(count.saturating_sub(1));
                Action::None
            }
            // ←/→ 切换页面（与 Tab 一致）；**设置页里改成切换三个分区**
            Key::Left => {
                if self.page == Page::Settings {
                    self.settings_section = self.settings_section.previous();
                    Action::None
                } else {
                    self.page = self.page.previous();
                    Action::Refresh
                }
            }
            Key::Right => {
                if self.page == Page::Settings {
                    self.settings_section = self.settings_section.next();
                    Action::None
                } else {
                    self.page = self.page.next();
                    Action::Refresh
                }
            }
            Key::Char('[') => self.shift_period(-1),
            Key::Char(']') => self.shift_period(1),
            Key::Char('r') => Action::Refresh,
            Key::Char('a') => {
                self.open_default_form();
                Action::None
            }
            Key::Char('e') => {
                self.open_edit_form();
                Action::None
            }
            Key::Char('i') => {
                // 设置页：建库 + 建表（初始化）
                if self.page == Page::Settings {
                    Action::InitDatabase
                } else {
                    Action::None
                }
            }
            Key::Char('t') => {
                // 设置页：把当前库的数据整库迁移到另一个库
                if self.page == Page::Settings {
                    self.open_migrate_form();
                }
                Action::None
            }
            Key::Char('u') => Action::UndoDelete,
            Key::Char('x') => {
                // 账单页：切「是否纳入统计」；报销、代付这类过手钱不该算进开销
                if self.page == Page::Bills {
                    self.bills
                        .get(self.selected_index(Page::Bills))
                        .map(|bill| Action::ToggleExcluded(bill.id))
                        .unwrap_or(Action::None)
                } else {
                    Action::None
                }
            }
            Key::Char('d') | Key::Delete => {
                self.request_delete();
                Action::None
            }
            Key::Char('m') => {
                let today = Self::today();
                // 月份统一用「当月 1 号」表示；直接存今天会让 list_in_month 一笔都查不到
                self.month = crate::common::time::first_day_of_month(today);
                self.year = today.year();
                // 回本月顺手把查看的日期也对到今天
                self.selected_day = today.day();
                Action::Refresh
            }
            _ => Action::None,
        }
    }

    fn on_key_modal(&mut self, key: Key) -> Action {
        let Some(modal) = self.modal.take() else {
            return Action::None;
        };

        match modal {
            Modal::Notice { title, message } => {
                // 除 Esc 外的任意键都关掉提示
                if key != Key::Esc {
                    return Action::None;
                }
                self.modal = Some(Modal::Notice { title, message });
                Action::None
            }
            Modal::Confirm {
                title,
                message,
                action,
            } => match key {
                Key::Enter | Key::Char('y') | Key::Char('Y') => action,
                Key::Esc | Key::Char('n') | Key::Char('N') => Action::None,
                _ => {
                    self.modal = Some(Modal::Confirm {
                        title,
                        message,
                        action,
                    });
                    Action::None
                }
            },
            Modal::Form(mut form) => {
                let mut keep_open = true;
                let mut action = Action::None;
                // 需要展开选择列表时先记录下来，避免在这里 move 掉 form
                let mut open_picker: Option<(usize, &'static str, Vec<String>, usize)> = None;

                match key {
                    Key::Esc => keep_open = false,
                    Key::Enter => {
                        // 连接设置与数据迁移都要重建连接池，交给外壳执行
                        if matches!(form.kind, FormKind::Connection | FormKind::MigrateData) {
                            match connection_from_form(&form) {
                                Ok(settings) => {
                                    keep_open = false;
                                    action = if form.kind == FormKind::Connection {
                                        Action::SaveConnection(settings)
                                    } else {
                                        Action::MigrateData(settings)
                                    };
                                }
                                Err(error) => self.set_error(format!("{error}")),
                            }
                        } else if form.current_is_choice() {
                            // 选择框上回车 = 展开候选列表；文本框上回车 = 提交表单
                            let field = form.focus;
                            let label = form.fields[field].label;
                            let options = form.fields[field].options().to_vec();

                            if options.is_empty() {
                                keep_open = false;
                                action = Action::SubmitForm(form.clone());
                            } else {
                                let selected = options
                                    .iter()
                                    .position(|option| *option == form.fields[field].value)
                                    .unwrap_or(0);
                                keep_open = false;
                                open_picker = Some((field, label, options, selected));
                            }
                        } else {
                            keep_open = false;
                            action = Action::SubmitForm(form.clone());
                        }
                    }
                    Key::Tab | Key::Down => form.next_field(),
                    Key::BackTab | Key::Up => form.previous_field(),
                    Key::Left | Key::Right => form.cycle_current(),
                    Key::Backspace => form.backspace(),
                    Key::Char(' ') => {
                        // 空的日期字段：空格 = 填入上次使用过的日期
                        let can_fill = form.fields.get(form.focus).is_some_and(|field| {
                            field.role == FieldRole::Date && field.value.trim().is_empty()
                        });

                        if form.current_is_choice() {
                            form.cycle_current();
                        } else if can_fill {
                            match self.last_entry_date {
                                Some(date) => {
                                    if let Some(field) = form.fields.get_mut(form.focus) {
                                        field.value = date.to_string();
                                    }
                                    self.set_info(format!("已填入上次使用的日期 {date}"));
                                }
                                None => self.set_info("还没有上次使用的时间；留空表示今天"),
                            }
                        } else {
                            form.push(' ');
                        }
                    }
                    Key::Char('=') => {
                        // 金额字段：按 = 把算式求值成结果
                        let is_amount = form
                            .fields
                            .get(form.focus)
                            .is_some_and(|field| field.role == FieldRole::Amount);

                        if !is_amount {
                            form.push('=');
                        } else {
                            let raw = form.fields[form.focus].value.trim().to_string();
                            if raw.is_empty() {
                                self.set_info("金额为空，先输入数字或算式");
                            } else {
                                match money::evaluate_expression(&raw) {
                                    Ok(result) => {
                                        form.fields[form.focus].value = result.clone();
                                        self.set_info(format!("= {result}"));
                                    }
                                    Err(error) => self.set_error(format!("{error}")),
                                }
                            }
                        }
                    }
                    Key::Char(character) => form.push(character),
                    _ => {}
                }

                if let Some((field, label, options, selected)) = open_picker {
                    self.modal = Some(Modal::Picker {
                        form,
                        field,
                        label,
                        options,
                        selected,
                    });
                } else if keep_open {
                    self.modal = Some(Modal::Form(form));
                }

                action
            }
            Modal::Picker {
                form,
                field,
                label,
                options,
                selected,
            } => {
                let mut form = form;
                let mut selected = selected;
                let mut confirmed = false;
                let mut cancelled = false;

                match key {
                    Key::Esc => cancelled = true,
                    Key::Enter => confirmed = true,
                    Key::Up => selected = selected.saturating_sub(1),
                    Key::Down => {
                        if selected + 1 < options.len() {
                            selected += 1;
                        }
                    }
                    Key::PageUp => selected = selected.saturating_sub(10),
                    Key::PageDown => {
                        selected = (selected + 10).min(options.len().saturating_sub(1));
                    }
                    Key::Home => selected = 0,
                    Key::End => selected = options.len().saturating_sub(1),
                    _ => {}
                }

                if confirmed {
                    if let Some(option) = options.get(selected) {
                        form.fields[field].value = option.clone();
                    }
                    // 选完自动跳到下一个字段，便于一路填完直接回车提交
                    form.next_field();
                    self.modal = Some(Modal::Form(form));
                } else if cancelled {
                    self.modal = Some(Modal::Form(form));
                } else {
                    self.modal = Some(Modal::Picker {
                        form,
                        field,
                        label,
                        options,
                        selected,
                    });
                }

                Action::None
            }
        }
    }

    /// 执行外壳给出的动作，并把错误显示成弹窗而不是中断界面。
    pub async fn apply(&mut self, action: Action, services: &Services) {
        if let Err(error) = self.apply_inner(action, services).await {
            self.modal = Some(Modal::Notice {
                title: "操作失败".to_string(),
                message: format!("{error:#}"),
            });
        }
    }

    async fn apply_inner(&mut self, action: Action, services: &Services) -> Result<()> {
        match action {
            Action::None => {}
            // 这些动作需要重建连接池，由外壳（tui/mod.rs）处理
            Action::SaveConnection(_) | Action::InitDatabase | Action::MigrateData(_) => {}
            Action::Refresh => self.reload(services).await?,
            Action::ShiftMonth(delta) => {
                self.month = shift_month(self.month, delta);
                self.reload(services).await?;
            }
            Action::ShiftYear(delta) => {
                self.year += delta;
                self.reload(services).await?;
            }
            Action::SubmitForm(form) => {
                let message = submit(form, services).await?;
                self.set_info(message);
                self.reload(services).await?;
            }
            Action::DeleteBill(id) => {
                // 删之前先把整条记下来，`u` 才有东西可恢复
                let record = self.bill_record(id);

                services.bills.remove(id).await?;
                self.last_deleted = record;
                self.set_info(format!("已删除账单 #{id}（按 u 撤销）"));
                self.reload(services).await?;
            }
            Action::DeleteBook(id) => {
                let book = services.books.remove(&id.to_string()).await?;
                self.last_deleted = Some(DeletedRecord::Book {
                    name: book.name.clone(),
                });
                self.set_info(format!("已删除账本「{}」（按 u 撤销）", book.name));
                self.reload(services).await?;
            }
            Action::DeleteCategory { id, recursive } => {
                let removed = services
                    .categories
                    .remove(&id.to_string(), recursive)
                    .await?;
                let root = removed.first().expect("至少包含目标自身");

                if removed.len() > 1 {
                    self.set_info(format!(
                        "已删除分类「{}」及其 {} 个子分类（按 u 撤销）",
                        root.name,
                        removed.len() - 1
                    ));
                } else {
                    self.set_info(format!("已删除分类「{}」（按 u 撤销）", root.name));
                }

                let record = self.category_record(&removed);
                self.last_deleted = Some(record);
                self.reload(services).await?;
            }
            Action::ToggleExcluded(id) => {
                let next = self
                    .bills
                    .iter()
                    .find(|bill| bill.id == id)
                    .map(|bill| !bill.excluded);

                match next {
                    None => self.set_error(format!("账单 #{id} 不在当前列表里")),
                    Some(next) => {
                        services.bills.set_excluded(id, next).await?;
                        self.reload(services).await?;
                        self.set_info(if next {
                            format!("账单 #{id} 已标为「不计入日历」")
                        } else {
                            format!("账单 #{id} 已恢复计入日历")
                        });
                    }
                }
            }
            Action::UndoDelete => match self.last_deleted.take() {
                None => self.set_info("没有可撤销的删除"),
                Some(DeletedRecord::Bill(bill)) => {
                    // 内容原样回填，但 id 是新的：恢复走的是普通新增，不做主键回插
                    let (restored, _) = services
                        .bills
                        .add(NewBill {
                            kind: bill.kind,
                            amount: bill.amount,
                            book: bill.book.name.clone(),
                            category: bill.category.name.clone(),
                            created_at: bill.created_at,
                            remark: bill.remark.clone(),
                        })
                        .await?;
                    self.reload(services).await?;
                    self.set_info(format!("已恢复账单 #{}（新 id）", restored.id));
                }
                Some(DeletedRecord::Book { name }) => {
                    let book = services.books.add(&name).await?;
                    self.reload(services).await?;
                    self.set_info(format!("已恢复账本「{}」（新 id #{}）", book.name, book.id));
                }
                Some(DeletedRecord::Categories(entries)) => {
                    let mut restored = 0;
                    for entry in &entries {
                        services
                            .categories
                            .add(&entry.name, entry.parent_name.as_deref())
                            .await?;
                        restored += 1;
                    }
                    self.reload(services).await?;
                    if restored == 1 {
                        self.set_info(format!("已恢复分类「{}」", entries[0].name));
                    } else {
                        self.set_info(format!("已恢复 {restored} 个分类"));
                    }
                }
            },
        }
        Ok(())
    }

    /// 重新读取当前页需要的数据。
    pub async fn reload(&mut self, services: &Services) -> Result<()> {
        self.books = services.books.list().await?;
        self.categories = services.categories.tree().await?;
        self.bills = services.bills.list_in_month(self.month).await?;
        self.report = Some(services.months.report(self.month).await?);
        self.year_report = Some(services.months.year_report(self.year).await?);
        self.last_entry_date = services.bills.last_used_date().await?;
        self.last_entry_book = services.bills.default_entry_book().await?;

        // 计划是全局的，进度按当前查看的月份算
        self.plan = services.plans.get().await?;
        let today = if self.viewing_current_month() {
            Some(Self::today())
        } else {
            None
        };
        self.progress = Some(
            services
                .plans
                .progress(self.month, self.plan, today, &self.bills),
        );
        self.clamp_selection();
        Ok(())
    }
}

/// 提交表单，返回给状态栏的提示语。
async fn submit(form: Form, services: &Services) -> Result<String> {
    match (form.kind, form.target) {
        (FormKind::NewBill, Some(EditTarget::Bill(id))) => {
            let bill = services.bills.update(id, bill_from_form(&form)?).await?;

            // 「计入统计」在编辑表单里也能改，单独落一次（幂等）
            services
                .bills
                .set_excluded(id, form.value("计入日历") == "不计入")
                .await?;

            Ok(format!(
                "已更新账单 #{} 为 {} {} 元",
                bill.id,
                bill.kind,
                money::format_amount(bill.amount)
            ))
        }
        (FormKind::NewBook, Some(EditTarget::Book(id))) => {
            let name = require(form.value("名称"), "账本名称不能为空")?;
            let book = services.books.rename(id, name).await?;
            Ok(format!("账本已改名为「{}」", book.name))
        }
        (FormKind::NewCategory, Some(EditTarget::Category(id))) => {
            let name = require(form.value("名称"), "分类名称不能为空")?;
            let parent = parent_from_form(&form);
            let category = services
                .categories
                .update(id, name, parent.as_deref())
                .await?;
            Ok(format!("分类「{}」已更新", category.name))
        }
        (FormKind::SetStartBalance, Some(EditTarget::MonthBalance(month))) => {
            let amount = money::parse_amount(form.value("起始金额"))
                .map_err(|error| anyhow!("{error}"))?;
            let plan = SpendingPlan::new(
                money::parse_amount(form.value("预计金额")).map_err(|error| anyhow!("{error}"))?,
                money::parse_amount(form.value("攒钱金额")).map_err(|error| anyhow!("{error}"))?,
            )
            .with_mode(match form.value("超支判断") {
                "只看当天" => PlanMode::DailyOnly,
                _ => PlanMode::Cumulative,
            });

            services.months.set_start_balance(month, amount).await?;
            services.plans.save(plan).await?;

            Ok(format!(
                "{} 起始金额 {} 元；计划上限 {} 元（{}）",
                crate::common::time::format_month(month),
                money::format_amount(amount),
                money::format_amount(plan.limit()),
                plan.mode.label()
            ))
        }
        (FormKind::NewBill, None) => {
            // 与「编辑账单」共用同一套表单解析，避免两处逻辑跑偏
            let (bill, auto_created) = services.bills.add(bill_from_form(&form)?).await?;

            let mut message = format!(
                "已记录 #{} {} {} 元",
                bill.id,
                bill.kind,
                money::format_amount(bill.amount)
            );
            if !auto_created.is_empty() {
                message.push_str(&format!("（自动创建{}）", auto_created.join("、")));
            }
            Ok(message)
        }
        (FormKind::NewBook, None) => {
            let name = form.value("名称");
            if name.is_empty() {
                bail!("账本名称不能为空");
            }
            let book = services.books.add(name).await?;
            Ok(format!("已新增账本「{}」", book.name))
        }
        (FormKind::NewCategory, None) => {
            let name = form.value("名称");
            if name.is_empty() {
                bail!("分类名称不能为空");
            }
            let parent = parent_from_form(&form);
            let category = services.categories.add(name, parent.as_deref()).await?;
            Ok(format!("已新增分类「{}」", category.name))
        }
        _ => bail!("表单类型与编辑目标不匹配"),
    }
}

/// 已有数据时用选择框（←/→ 切换），没有数据时退化成文本框（输入即自动创建）。
///
/// `preferred` 是上次用过的值，存在于候选里时作为默认选中项。
fn picker(
    label: &'static str,
    options: Vec<String>,
    preferred: Option<&str>,
    hint: &'static str,
) -> Field {
    if options.is_empty() {
        Field::text(label, "", "暂无数据，直接输入名称将自动创建")
    } else {
        Field::choice_with(
            label,
            options,
            preferred.unwrap_or_default().to_string(),
            hint,
        )
    }
}

/// 把字段值设为 `name`。
///
/// 选择框会在选项里找匹配项（忽略缩进）并取用选项原文，避免选项里
/// 带层级缩进时循环切换对不上；找不到或本身就是文本框时直接写入。
fn set_field_value(field: &mut Field, name: &str) {
    if let FieldKind::Choice(options) = &field.kind {
        if let Some(matched) = options.iter().find(|option| option.trim() == name) {
            field.value = matched.clone();
            return;
        }
    }
    field.value = name.to_string();
}

/// 从表单构造账单入参。
fn bill_from_form(form: &Form) -> Result<NewBill> {
    let kind = match form.value("方向") {
        "收入" => BillKind::Income,
        _ => BillKind::Expense,
    };
    // 直接提交算式也等价于按过 `=`；纯数字仍走原解析，保留「最多两位小数」的校验
    let raw_amount = form.value("金额");
    let amount = if raw_amount
        .chars()
        .any(|character| "+-*/()".contains(character))
    {
        let evaluated =
            money::evaluate_expression(raw_amount).map_err(|error| anyhow!("{error}"))?;
        money::parse_amount(&evaluated).map_err(|error| anyhow!("{error}"))?
    } else {
        money::parse_amount(raw_amount).map_err(|error| anyhow!("{error}"))?
    };

    let date = form.value("日期");
    let created_at = if date.is_empty() {
        Utc::now()
    } else {
        crate::common::time::parse_datetime(Some(date)).map_err(|error| anyhow!("{error}"))?
    };

    Ok(NewBill {
        kind,
        amount,
        book: require(form.value("账本"), "账本名称不能为空")?.to_string(),
        category: require(form.value("分类"), "分类名称不能为空")?.to_string(),
        created_at,
        remark: match form.value("备注") {
            "" => None,
            text => Some(text.to_string()),
        },
    })
}

/// 父分类字段 → 名称。
///
/// 表单里的选项带层级缩进，而 `Form::value` 会去掉首尾空白，
/// 因此这里拿到的就是纯名称；`(顶层)` 表示不挂父分类。
fn parent_from_form(form: &Form) -> Option<String> {
    match form.value("父分类") {
        "" | "(顶层)" => None,
        name => Some(name.to_string()),
    }
}

/// 从表单构造连接设置，顺带做基本校验。
fn connection_from_form(form: &Form) -> Result<ConnectionSettings> {
    let host = require(form.value("主机"), "主机不能为空")?.to_string();
    let user = require(form.value("用户名"), "用户名不能为空")?.to_string();
    let database = require(form.value("数据库"), "数据库名不能为空")?.to_string();

    let port_text = form.value("端口");
    let port: u16 = port_text
        .parse()
        .map_err(|_| anyhow!("端口必须是 1-65535 之间的数字：{port_text}"))?;
    if port == 0 {
        bail!("端口必须是 1-65535 之间的数字");
    }

    Ok(ConnectionSettings {
        host,
        port,
        user,
        password: form.value("密码").to_string(),
        database,
    })
}

fn require<'a>(value: &'a str, message: &str) -> Result<&'a str> {
    if value.is_empty() {
        bail!("{message}");
    }
    Ok(value)
}

/// 月份加减，越界时保持不变。
fn shift_month(month: NaiveDate, delta: i32) -> NaiveDate {
    let magnitude = Months::new(delta.unsigned_abs());
    let shifted = if delta >= 0 {
        month.checked_add_months(magnitude)
    } else {
        month.checked_sub_months(magnitude)
    };
    shifted.unwrap_or(month)
}

/// 账单页当前月份的查询条件（供外部复用）。
pub fn month_query(month: NaiveDate) -> BillQuery {
    BillQuery {
        month: Some(month),
        limit: Some(500),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::month::model::BillMonthCollection;

    /// 测试用连接串：各字段值都便于断言。
    const TEST_CONNECTION_URL: &str = "postgres://postgres:postgres@127.0.0.1:5432/biller";

    fn sample_app() -> App {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut app = App::new(month, TEST_CONNECTION_URL);

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
        app.bills = vec![sample_bill(7, 1234)];

        // 概览/月报页需要报表：起始金额用来验证「e 改起始金额」的预填
        let mut collection = BillMonthCollection::new(month, 100_000);
        collection.total_income = 800_000;
        collection.total_expense = 2_084;
        collection.expense_by_category.insert(1, 1234);
        collection.income_by_category.insert(3, 800_000);
        // 月报的支出排行按「分类 + 账本」成对，这里也得给一条
        collection.expense_by_category_book.insert((1, 1), 1234);

        let mut category_names = HashMap::new();
        category_names.insert(1, "餐饮".to_string());
        category_names.insert(3, "工资".to_string());

        let mut book_names = HashMap::new();
        book_names.insert(1, "微信".to_string());

        app.report = Some(MonthReport {
            collection,
            book_names,
            category_names,
        });

        app
    }

    #[test]
    fn tabs_cycle_pages() {
        let mut app = sample_app();
        assert_eq!(app.page, Page::Overview);
        app.on_key(Key::Tab);
        assert_eq!(app.page, Page::Bills);
        app.on_key(Key::BackTab);
        assert_eq!(app.page, Page::Overview);
        app.on_key(Key::BackTab);
        assert_eq!(
            app.page,
            *Page::ALL.last().expect("至少有一页"),
            "从第一页往前应回绕到最后一页"
        );
    }

    #[test]
    fn number_keys_jump_to_page() {
        let mut app = sample_app();
        app.on_key(Key::Char('4'));
        assert_eq!(app.page, Page::Years, "第 4 页是年报");
        app.on_key(Key::Char('5'));
        assert_eq!(app.page, Page::Settings, "第 5 页是设置");
        app.on_key(Key::Char('9'));
        assert_eq!(app.page, Page::Settings, "越界数字应被忽略");
    }

    #[test]
    fn quit_keys() {
        let mut app = sample_app();
        app.on_key(Key::Char('q'));
        assert!(app.should_quit);

        let mut app = sample_app();
        app.on_key(Key::Ctrl('c'));
        assert!(app.should_quit);
    }

    #[test]
    fn month_shifts_with_brackets() {
        let mut app = sample_app();
        assert!(matches!(app.on_key(Key::Char(']')), Action::ShiftMonth(1)));
        assert!(matches!(app.on_key(Key::Char('[')), Action::ShiftMonth(-1)));

        app.month = shift_month(app.month, 1);
        assert_eq!(app.month, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap());
        app.month = shift_month(app.month, -1);
        assert_eq!(app.month, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        app.month = shift_month(app.month, -10);
        assert_eq!(app.month, NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
    }

    #[test]
    fn arrow_keys_move_day_on_overview_and_switch_page_elsewhere() {
        let mut app = sample_app();
        let month = app.month;

        // 概览页：↑↓ 移动正在查看的日期
        let day = app.inspected_day();
        assert!(matches!(app.on_key(Key::Down), Action::Refresh));
        assert_eq!(app.page, Page::Overview, "概览页的 ↓ 不换页");
        assert_eq!(
            app.inspected_day(),
            (day + 1).min(days_in_month(month)),
            "日期应往后挪一天"
        );
        app.on_key(Key::Up);
        assert_eq!(app.inspected_day(), day);

        // ←→ 在任何页面都还是换页
        app.on_key(Key::Right);
        assert_eq!(app.page, Page::Bills);
        app.on_key(Key::Right);
        assert_eq!(app.page, Page::Months);
        app.on_key(Key::Left);
        assert_eq!(app.page, Page::Bills);
        app.on_key(Key::Left);
        assert_eq!(app.page, Page::Overview);

        assert_eq!(app.month, month, "箭头不应改变月份");
    }

    /// 回归：`m` 必须把月份设成「当月 1 号」。
    ///
    /// 曾经直接把今天的日期塞进 `month`，于是 `list_in_month` 的月份匹配永远不成立，
    /// 一笔账单都查不到；再用 `[`/`]` 加减月份也是从那天算，永远回不来，只能重启。
    #[test]
    fn m_returns_to_first_day_of_current_month() {
        let mut app = sample_app();
        app.month = NaiveDate::from_ymd_opt(2025, 3, 17).unwrap();
        app.year = 2025;

        assert!(matches!(app.on_key(Key::Char('m')), Action::Refresh));

        let today = App::today();
        assert_eq!(app.month, crate::common::time::first_day_of_month(today));
        assert_eq!(app.month.day(), 1, "月份必须落在 1 号，否则按月查询会查空");
        assert_eq!(app.year, today.year());
        assert_eq!(app.inspected_day(), today.day());
    }

    #[test]
    fn bill_form_uses_existing_books_and_categories() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        match &app.modal {
            Some(Modal::Form(form)) => {
                assert!(form.fields[2].is_choice(), "账本应自动读取成选择框");
                assert!(form.fields[3].is_choice(), "分类应自动读取成选择框");
                assert_eq!(form.value("账本"), "微信");
                assert_eq!(form.value("分类"), "餐饮");
            }
            other => panic!("应打开新增账单表单，实际 {other:?}"),
        }
    }

    #[test]
    fn bill_form_falls_back_to_text_on_empty_database() {
        let mut app = App::new(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), TEST_CONNECTION_URL);
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        match &app.modal {
            Some(Modal::Form(form)) => {
                assert!(!form.fields[2].is_choice(), "没有账本时应退化成文本框");
                assert!(!form.fields[3].is_choice(), "没有分类时应退化成文本框");
            }
            other => panic!("应打开新增账单表单，实际 {other:?}"),
        }
    }

    #[test]
    fn new_bill_form_focuses_book_and_leaves_date_empty() {
        let form = Form::new_bill(vec!["微信".to_string()], vec!["餐饮".to_string()], None);

        assert_eq!(form.focus, 2, "焦点应默认落在「账本」");
        assert_eq!(form.fields[2].label, "账本");
        assert_eq!(form.value("账本"), "微信");
        assert_eq!(form.fields[4].value, "", "日期默认留空，留空即今天");
        assert_eq!(
            form.fields[4].role,
            FieldRole::Date,
            "日期字段的空格应用来填入上次使用的时间"
        );
    }

    #[test]
    fn new_bill_form_prefers_last_used_book() {
        let books = vec!["微信".to_string(), "支付宝".to_string()];
        let categories = vec!["餐饮".to_string()];

        // 上次用的是「支付宝」→ 默认应选中它，而不是列表第一个
        let form = Form::new_bill(books.clone(), categories.clone(), Some("支付宝"));
        assert_eq!(form.value("账本"), "支付宝");

        // 上次那本账本已被删除 → 回落到第一个
        let form = Form::new_bill(books, categories, Some("已删除的账本"));
        assert_eq!(form.value("账本"), "微信");
    }

    #[test]
    fn space_on_empty_date_field_fills_last_used_date() {
        let mut app = sample_app();
        app.last_entry_date = Some(NaiveDate::from_ymd_opt(2026, 9, 30).unwrap());
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        // 焦点：账本(2) → 分类(3) → 日期(4)
        app.on_key(Key::Tab);
        app.on_key(Key::Tab);
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.focus, 4);
                assert_eq!(form.fields[4].value, "", "日期一开始应为空");
            }
            other => panic!("{other:?}"),
        }

        // 空格 → 填入上次使用过的日期
        app.on_key(Key::Char(' '));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.fields[4].value, "2026-09-30");
            }
            other => panic!("{other:?}"),
        }

        // 已有内容时空格仍是普通空格，方便手输 "日期 时间"
        app.on_key(Key::Char(' '));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.fields[4].value, "2026-09-30 ");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn space_on_date_field_without_memory_only_hints() {
        let mut app = sample_app();
        app.last_entry_date = None;
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));
        app.on_key(Key::Tab);
        app.on_key(Key::Tab);
        app.on_key(Key::Char(' '));

        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.fields[4].value, "", "没有记忆时不应乱填");
            }
            other => panic!("{other:?}"),
        }
        assert!(app.status.is_some(), "应给出「还没有上次使用的时间」的提示");
    }

    #[test]
    fn category_delete_confirmation_lists_subcategories() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.settings_selected[SettingsSection::Categories.index()] = 0; // 餐饮（有子分类 午餐）
        app.on_key(Key::Char('d'));

        match &app.modal {
            Some(Modal::Confirm { message, action, .. }) => {
                assert!(message.contains("餐饮"), "{message}");
                assert!(message.contains("子分类"), "应提示会连带删除子分类：{message}");
                assert!(message.contains("午餐"), "应列出子分类名称：{message}");
                assert!(
                    matches!(action, Action::DeleteCategory { recursive: true, .. }),
                    "有子分类时应标记为递归删除"
                );
            }
            other => panic!("应弹出确认框，实际 {other:?}"),
        }
    }

    #[test]
    fn category_delete_on_leaf_is_not_recursive() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.settings_selected[SettingsSection::Categories.index()] = 1; // 午餐（叶子）
        app.on_key(Key::Char('d'));

        match &app.modal {
            Some(Modal::Confirm { message, action, .. }) => {
                assert!(message.contains("午餐"), "{message}");
                assert!(matches!(
                    action,
                    Action::DeleteCategory { recursive: false, .. }
                ));
            }
            other => panic!("应弹出确认框，实际 {other:?}"),
        }
    }

    #[test]
    fn add_opens_form_and_form_edits() {
        let mut app = sample_app();
        app.on_key(Key::Char('a'));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.kind, FormKind::NewBill),
            other => panic!("应打开新增账单表单，实际 {other:?}"),
        }

        // 焦点默认在「账本」，先移到「金额」
        app.on_key(Key::BackTab);
        app.on_key(Key::Char('1'));
        app.on_key(Key::Char('2'));
        app.on_key(Key::Char('.'));
        app.on_key(Key::Char('3'));
        app.on_key(Key::Char('4'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.value("金额"), "12.34");
                assert_eq!(form.value("方向"), "支出");
            }
            other => panic!("表单应仍然打开，实际 {other:?}"),
        }

        // 退格
        app.on_key(Key::Backspace);
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("金额"), "12.3"),
            other => panic!("表单应仍然打开，实际 {other:?}"),
        }
        app.on_key(Key::Esc);
        assert!(app.modal.is_none(), "Esc 应关闭表单");
    }

    #[test]
    fn choice_field_cycles_without_typing() {
        let mut app = sample_app();
        app.on_key(Key::Char('a'));
        // 焦点默认在「账本」，退回两格到「方向」再切换
        app.on_key(Key::BackTab);
        app.on_key(Key::BackTab);
        app.on_key(Key::Left);
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("方向"), "收入"),
            other => panic!("表单应仍然打开，实际 {other:?}"),
        }
        // 选择型字段不应接受字符输入
        app.on_key(Key::Char('x'));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("方向"), "收入"),
            other => panic!("表单应仍然打开，实际 {other:?}"),
        }
    }

    #[test]
    fn submit_action_is_emitted_on_enter() {
        let mut app = sample_app();
        app.on_key(Key::Char('a'));
        // 焦点默认在「账本」选择框上，回车会展开列表；先退到文本框再回车提交
        app.on_key(Key::BackTab);
        let action = app.on_key(Key::Enter);
        match action {
            Action::SubmitForm(form) => assert_eq!(form.kind, FormKind::NewBill),
            other => panic!("Enter 应提交表单，实际 {other:?}"),
        }
        assert!(app.modal.is_none(), "提交后表单应关闭");
    }

    #[test]
    fn delete_asks_for_confirmation_and_targets_selected_row() {
        let mut app = sample_app();
        app.bills = vec![
            sample_bill(7, 1000),
            sample_bill(9, 2000),
        ];
        app.page = Page::Bills;
        app.selected[Page::Bills.index()] = 1;

        app.on_key(Key::Char('d'));
        let action = match &app.modal {
            Some(Modal::Confirm { action, .. }) => action.clone(),
            other => panic!("应弹出确认框，实际 {other:?}"),
        };
        assert!(matches!(action, Action::DeleteBill(9)), "应针对选中行");

        // n 取消
        let cancelled = app.on_key(Key::Char('n'));
        assert!(matches!(cancelled, Action::None));
        assert!(app.modal.is_none());

        // y 确认
        app.on_key(Key::Char('d'));
        let confirmed = app.on_key(Key::Char('y'));
        assert!(matches!(confirmed, Action::DeleteBill(9)));
    }

    #[test]
    fn delete_without_selection_only_reports() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.bills.clear();
        let action = app.on_key(Key::Char('d'));
        assert!(matches!(action, Action::None));
        assert!(app.modal.is_none());
        assert!(app.status.is_some());
    }

    #[test]
    fn selection_wraps_and_is_clamped() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;
        app.books = vec![
            BillBook { id: 1, name: "微信".into() },
            BillBook { id: 2, name: "支付宝".into() },
        ];
        app.on_key(Key::Up);
        assert_eq!(app.settings_index(), 1, "向上应回绕到末行");
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 0, "向下应回绕到首行");

        app.books.truncate(1);
        app.clamp_selection();
        assert_eq!(app.settings_index(), 0);
    }

    #[test]
    fn a_key_opens_page_specific_form() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;
        app.on_key(Key::Char('a'));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.kind, FormKind::NewBook),
            other => panic!("账本页应打开新增账本，实际 {other:?}"),
        }

        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.on_key(Key::Char('a'));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.kind, FormKind::NewCategory),
            other => panic!("分类页应打开新增分类，实际 {other:?}"),
        }
    }

    #[test]
    fn settings_sections_switch_with_arrows() {
        let mut app = sample_app();
        app.page = Page::Settings;

        assert_eq!(app.settings_section, SettingsSection::Connection);
        // 设置页里 ←→ 切分区，不换页面
        assert!(matches!(app.on_key(Key::Right), Action::None));
        assert_eq!(app.settings_section, SettingsSection::Books);
        assert_eq!(app.page, Page::Settings);

        app.on_key(Key::Right);
        assert_eq!(app.settings_section, SettingsSection::Categories);
        app.on_key(Key::Right);
        assert_eq!(app.settings_section, SettingsSection::Connection, "应回绕");
        app.on_key(Key::Left);
        assert_eq!(app.settings_section, SettingsSection::Categories);

        // 别的页面 ←→ 仍然是切页
        app.page = Page::Bills;
        assert!(matches!(app.on_key(Key::Right), Action::Refresh));
        assert_eq!(app.page, Page::Months);
    }

    #[test]
    fn settings_selection_is_per_section() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;

        assert_eq!(app.settings_row_count(), 2);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 1);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 0, "到底应回绕");

        // 分类分区有自己的选中行
        app.settings_section = SettingsSection::Categories;
        assert_eq!(app.settings_index(), 0);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 1);

        // 回到账本分区，选中行还是原来那个
        app.settings_section = SettingsSection::Books;
        assert_eq!(app.settings_index(), 0);

        // 连接分区没有列表，上下键无害
        app.settings_section = SettingsSection::Connection;
        assert_eq!(app.settings_row_count(), 0);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 0);
    }

    fn sample_bill(id: u64, amount: i64) -> BillEntity {
        BillEntity {
            id,
            kind: BillKind::Expense,
            amount,
            book: BillBook { id: 1, name: "微信".into() },
            category: Category { id: 1, parent_id: None, name: "餐饮".into() },
            created_at: Utc::now(),
            remark: None,
            excluded: false,
        }
    }

    #[test]
    fn e_opens_edit_form_prefilled() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.target, Some(EditTarget::Bill(7)));
                assert_eq!(form.value("方向"), "支出");
                assert_eq!(form.value("金额"), "12.34");
                assert_eq!(form.value("账本"), "微信");
                assert_eq!(form.value("分类"), "餐饮");
            }
            other => panic!("账单页应打开编辑表单，实际 {other:?}"),
        }

        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;
        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.target, Some(EditTarget::Book(1)));
                assert_eq!(form.value("名称"), "微信");
            }
            other => panic!("账本页应打开编辑表单，实际 {other:?}"),
        }

        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.target, Some(EditTarget::Category(1)));
                assert_eq!(form.value("名称"), "餐饮");
                assert_eq!(form.value("父分类"), "(顶层)");
            }
            other => panic!("分类页应打开编辑表单，实际 {other:?}"),
        }
    }

    #[test]
    fn parent_options_exclude_self_and_descendants() {
        let app = sample_app();

        // 新增分类时：全部候选，带缩进表示层级
        assert_eq!(
            app.parent_options(None),
            vec!["(顶层)".to_string(), "餐饮".to_string(), "  午餐".to_string()]
        );

        // 编辑「餐饮」：自己与子孙都不能当父分类
        assert_eq!(app.parent_options(Some(1)), vec!["(顶层)".to_string()]);

        // 编辑「午餐」：可以把父分类改成「餐饮」
        assert_eq!(
            app.parent_options(Some(2)),
            vec!["(顶层)".to_string(), "餐饮".to_string()]
        );
    }

    #[test]
    fn category_form_submits_indented_parent_as_plain_name() {
        // 表单值是带缩进的显示标签，提交时必须还原成纯名称
        let form = Form::new_category(vec![
            "(顶层)".to_string(),
            "餐饮".to_string(),
            "  午餐".to_string(),
        ]);
        assert_eq!(parent_from_form(&form), None, "默认应为顶层");

        let mut form = form;
        form.fields[1].cycle();
        form.fields[1].cycle();
        assert_eq!(form.value("父分类"), "午餐");
        assert_eq!(parent_from_form(&form), Some("午餐".to_string()));
    }

    #[test]
    fn overview_e_opens_start_balance_form() {
        let mut app = sample_app();
        app.page = Page::Overview;
        app.on_key(Key::Char('e'));

        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::SetStartBalance);
                assert_eq!(form.target, Some(EditTarget::MonthBalance(app.month)));
                assert_eq!(form.value("起始金额"), "1,000.00");
            }
            other => panic!("概览页应打开起始金额表单，实际 {other:?}"),
        }
    }

    #[test]
    fn enter_on_choice_field_expands_picker() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        // 新增账单时焦点默认就落在「账本」选择框上
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.focus, 2),
            other => panic!("{other:?}"),
        }

        let action = app.on_key(Key::Enter);
        assert!(
            matches!(action, Action::None),
            "选择框上回车应先展开候选，而不是提交表单"
        );
        match &app.modal {
            Some(Modal::Picker {
                options, selected, ..
            }) => {
                assert_eq!(options.len(), 2);
                assert_eq!(*selected, 0);
            }
            other => panic!("应展开选择列表，实际 {other:?}"),
        }

        // 选第二项并确认：值回填，焦点自动前进
        app.on_key(Key::Down);
        app.on_key(Key::Enter);
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.value("账本"), "支付宝");
                assert_eq!(form.focus, 3, "确认后应跳到下一个字段");
            }
            other => panic!("应回到表单，实际 {other:?}"),
        }

        // Esc 取消不应改动原值
        app.on_key(Key::Right);
        app.on_key(Key::Enter);
        app.on_key(Key::Down);
        app.on_key(Key::Esc);
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("账本"), "支付宝"),
            other => panic!("应回到表单，实际 {other:?}"),
        }
    }

    #[test]
    fn brackets_switch_year_on_year_page_only() {
        let mut app = sample_app();
        let month = app.month;
        let year = app.year;

        app.page = Page::Years;
        assert!(matches!(app.on_key(Key::Char(']')), Action::ShiftYear(1)));
        assert!(matches!(app.on_key(Key::Char('[')), Action::ShiftYear(-1)));
        assert_eq!(app.month, month, "年报页换年不应动月份");
        assert_eq!(app.year, year, "动作由外壳执行，状态机本身不变更年份");

        app.page = Page::Months;
        assert!(matches!(app.on_key(Key::Char(']')), Action::ShiftMonth(1)));
    }

    #[test]
    fn equals_evaluates_amount_expression() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        // 焦点默认在「账本」，退一格到「金额」
        app.on_key(Key::BackTab);
        for character in "12.5+7.5".chars() {
            app.on_key(Key::Char(character));
        }
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("金额"), "12.5+7.5"),
            other => panic!("{other:?}"),
        }

        app.on_key(Key::Char('='));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.value("金额"), "20.00", "按 = 应把算式求值成结果");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn equals_on_amount_field_keeps_value_when_expression_is_bad() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));
        app.on_key(Key::BackTab);

        for character in "12/0".chars() {
            app.on_key(Key::Char(character));
        }
        app.on_key(Key::Char('='));

        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.value("金额"), "12/0", "求值失败应保持原样");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            app.status.as_ref().is_some_and(|status| status.is_error()),
            "求值失败应给出错误提示"
        );
    }

    #[test]
    fn equals_on_plain_field_just_inserts_character() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;
        app.on_key(Key::Char('a'));

        app.on_key(Key::Char('='));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("名称"), "="),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn settings_page_edits_connection() {
        let mut app = sample_app();
        app.page = Page::Settings;

        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::Connection);
                assert_eq!(form.value("主机"), "127.0.0.1");
                assert_eq!(form.value("端口"), "5432");
                assert_eq!(form.value("用户名"), "postgres");
                assert_eq!(form.value("密码"), "postgres");
                assert_eq!(form.value("数据库"), "biller");
            }
            other => panic!("设置页应打开连接表单，实际 {other:?}"),
        }

        // 焦点在「主机」，Tab 到「端口」后改成 6543
        app.on_key(Key::Tab);
        for _ in 0..4 {
            app.on_key(Key::Backspace);
        }
        for character in "6543".chars() {
            app.on_key(Key::Char(character));
        }

        let action = app.on_key(Key::Enter);
        match action {
            Action::SaveConnection(settings) => {
                assert_eq!(settings.host, "127.0.0.1");
                assert_eq!(settings.port, 6543);
                assert_eq!(settings.database, "biller");
            }
            other => panic!("回车应产出 SaveConnection，实际 {other:?}"),
        }
        assert!(app.modal.is_none(), "保存后表单应关闭");
    }

    #[test]
    fn connection_form_rejects_bad_port() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.on_key(Key::Char('e'));
        app.on_key(Key::Tab);

        for _ in 0..4 {
            app.on_key(Key::Backspace);
        }
        for character in "abc".chars() {
            app.on_key(Key::Char(character));
        }

        let action = app.on_key(Key::Enter);
        assert!(matches!(action, Action::None), "非法端口不应产出动作");
        assert!(app.modal.is_some(), "表单应保持打开");
        assert!(
            app.status.as_ref().is_some_and(|status| status.is_error()),
            "应给出端口非法的提示"
        );
    }

    #[test]
    fn settings_page_i_requests_database_init() {
        let mut app = sample_app();
        app.page = Page::Settings;
        assert!(matches!(app.on_key(Key::Char('i')), Action::InitDatabase));

        // 其它页面按 i 不触发
        app.page = Page::Bills;
        assert!(matches!(app.on_key(Key::Char('i')), Action::None));
    }

    #[test]
    fn settings_page_opens_migration_form() {
        let mut app = sample_app();
        app.page = Page::Settings;

        app.on_key(Key::Char('t'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::MigrateData);
                assert_eq!(form.value("数据库"), "biller", "默认预填当前连接");
            }
            other => panic!("设置页应打开迁移表单，实际 {other:?}"),
        }

        // 焦点在「主机」，Tab 四次到「数据库」，改成 biller_new
        for _ in 0..4 {
            app.on_key(Key::Tab);
        }
        for _ in 0.."biller".len() {
            app.on_key(Key::Backspace);
        }
        for character in "biller_new".chars() {
            app.on_key(Key::Char(character));
        }

        let action = app.on_key(Key::Enter);
        match action {
            Action::MigrateData(settings) => assert_eq!(settings.database, "biller_new"),
            other => panic!("回车应产出 MigrateData，实际 {other:?}"),
        }
        assert!(app.modal.is_none(), "提交后表单应关闭");
    }

    #[test]
    fn month_expense_pairs_are_split_by_category_and_book() {
        let mut app = sample_app();
        app.page = Page::Months;

        assert_eq!(
            app.month_expense_pairs(),
            vec![(1, 1, 1234)],
            "月报支出按「分类 + 账本」成对排行"
        );
        assert_eq!(app.month_selected_label().as_deref(), Some("餐饮 @ 微信"));
        assert_eq!(
            app.month_selected_bills().len(),
            1,
            "明细应只含选中那一对（分类 + 账本）的账单"
        );
    }

    #[test]
    fn today_totals_focus_on_today() {
        let today = App::today();
        let month = crate::common::time::first_day_of_month(today);

        let mut app = sample_app();
        app.month = month;
        app.report = Some(MonthReport {
            // 起始金额 100.00
            collection: BillMonthCollection::new(month, 10_000),
            book_names: HashMap::new(),
            category_names: HashMap::new(),
        });

        let mut today_expense = sample_bill(1, 2_000);
        today_expense.created_at = Utc::now();

        let mut today_income = sample_bill(2, 5_000);
        today_income.kind = BillKind::Income;
        today_income.created_at = Utc::now();

        // 记到未来日期的账单不该算进「截至今日」
        let mut future = sample_bill(3, 99_999);
        future.created_at = Utc::now() + chrono::Duration::days(1);

        app.bills = vec![today_expense, today_income, future];

        // 剩余 = 起始 100 + 收入 50 − 支出 20；今日开销 = 20
        assert_eq!(app.today_totals(), Some((13_000, 2_000)));
    }

    #[test]
    fn today_totals_are_absent_when_viewing_another_month() {
        let mut app = sample_app();

        // 把查看月份挪到很久以前，今日数据就不该再给出
        if let Some(report) = app.report.as_mut() {
            report.collection.month = NaiveDate::from_ymd_opt(2001, 1, 1).unwrap();
        }

        assert!(
            app.today_totals().is_none(),
            "查看的不是本月时不该给出今日数据"
        );
    }

    #[test]
    fn x_toggles_included_flag_on_bills_page_only() {
        let mut app = sample_app();
        app.page = Page::Bills;
        assert!(matches!(
            app.on_key(Key::Char('x')),
            Action::ToggleExcluded(7)
        ));

        app.page = Page::Settings;
        app.settings_section = SettingsSection::Books;
        assert!(matches!(app.on_key(Key::Char('x')), Action::None));
    }

    #[test]
    fn overview_e_edits_start_balance_and_plan_together() {
        let mut app = sample_app();
        app.plan = SpendingPlan::new(12_000_00, 8_000_00);

        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::SetStartBalance);
                assert_eq!(form.value("起始金额"), "1,000.00");
                assert_eq!(form.value("预计金额"), "12,000.00");
                assert_eq!(form.value("攒钱金额"), "8,000.00");
                assert_eq!(form.value("超支判断"), "累计标红");
            }
            other => panic!("概览页应打开本月设置，实际 {other:?}"),
        }
    }

    #[test]
    fn u_requests_undo() {
        let mut app = sample_app();
        assert!(matches!(app.on_key(Key::Char('u')), Action::UndoDelete));
    }

    #[test]
    fn delete_snapshots_the_row_for_undo() {
        let app = sample_app();

        match app.bill_record(7).expect("样例里有 #7") {
            DeletedRecord::Bill(bill) => {
                assert_eq!(bill.id, 7);
                assert_eq!(bill.amount, 1234);
                assert_eq!(bill.book.name, "微信");
            }
            other => panic!("应记录成账单快照，实际 {other:?}"),
        }

        assert!(app.bill_record(999).is_none(), "找不到的 id 不该记");
    }

    #[test]
    fn category_undo_record_puts_parents_first() {
        let app = sample_app();

        // 删除顺序是「子先父后」，恢复记录必须反过来，否则外键会拦住
        let removed = vec![
            Category {
                id: 2,
                parent_id: Some(1),
                name: "午餐".into(),
            },
            Category {
                id: 1,
                parent_id: None,
                name: "餐饮".into(),
            },
        ];

        match app.category_record(&removed) {
            DeletedRecord::Categories(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].name, "餐饮", "父分类要排前面");
                assert_eq!(entries[0].parent_name, None);
                assert_eq!(entries[1].name, "午餐");
                assert_eq!(
                    entries[1].parent_name.as_deref(),
                    Some("餐饮"),
                    "子分类要记住父分类的名字（id 已经没了）"
                );
            }
            other => panic!("应记录成分类树，实际 {other:?}"),
        }
    }
}
