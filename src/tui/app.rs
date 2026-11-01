//! 交互式 TUI 的状态机。
//!
//! 本模块只有纯逻辑：
//! 按键 → [`Action`]，以及 [`Action`] → 调用各领域 `service`。
//! 渲染与终端 IO 分别在 `view.rs` / `event.rs`，因此这里可以脱离终端测试。

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, Months, NaiveDate, Utc};

use crate::bank::model::BankTransaction;
use crate::bank::service::{BankService, Suggestion, SyncKind};
use crate::bill::model::{BillEntity, BillKind};
use crate::bill::service::{BillQuery, DEFAULT_BOOK, DEFAULT_CATEGORY, NewBill};
use crate::bill_book::model::{BillBook, PlanKind};
use crate::category::model::Category;
use crate::category::service::{CategoryNode, descendants_of, subtree_of};
use crate::common::db::ConnectionSettings;
use crate::common::money;
use crate::common::types::{AmountType, BookId, CategoryId};
use crate::month::service::{MonthReport, YearReport};
use crate::plan::model::{PlanCoverage, PlanProgress, PlanRow, days_in_month};
use crate::tui::Services;
use crate::tui::input::Key;

/// 页签。
///
/// 只有四个：概览、计划、账单、报表。月报与年报是同一个问题的两种时间尺度，
/// 合成一页（页内 `m` / `y` 切）；设置一年点不了几次，` , ` 随时进出，不占导航位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    /// 资金分配：这个月的钱分成哪几块、合起来能不能盖住收入。
    Plan,
    Bills,
    /// 报表：月报 + 年报。
    Reports,
    /// 设置不在页签队列里，` , ` 打开。
    Settings,
}

impl Page {
    /// 顶栏页签队列。设置不在里面。
    pub const ALL: [Page; 4] = [Page::Overview, Page::Plan, Page::Bills, Page::Reports];

    pub fn title(self) -> &'static str {
        match self {
            Page::Overview => "概览",
            Page::Plan => "计划",
            Page::Bills => "账单",
            Page::Reports => "报表",
            Page::Settings => "设置",
        }
    }

    /// 该页是否支持上下选择行。
    pub fn is_list(self) -> bool {
        matches!(self, Page::Plan | Page::Bills | Page::Reports)
    }

    pub fn index(self) -> usize {
        Page::ALL.iter().position(|page| *page == self).unwrap_or(0)
    }

    /// 选中行下标的槽位。设置不在页签里，单独占最后一格，免得和概览抢同一格。
    pub fn slot(self) -> usize {
        match self {
            Page::Settings => Page::ALL.len(),
            other => other.index(),
        }
    }

    /// 下一个页签。设置不在队列里，从它往右等于从队首开始。
    pub fn next(self) -> Self {
        match self {
            Page::Settings => Page::ALL[0],
            _ => Page::ALL[(self.index() + 1) % Page::ALL.len()],
        }
    }

    /// 上一个页签。设置不在队列里，从它往左等于回到队尾。
    pub fn previous(self) -> Self {
        match self {
            Page::Settings => Page::ALL[Page::ALL.len() - 1],
            _ => {
                let index = self.index();
                Page::ALL[(index + Page::ALL.len() - 1) % Page::ALL.len()]
            }
        }
    }
}

/// 报表页的两个时间尺度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportTab {
    /// 月报：本月分类 + 账本排行、明细。
    #[default]
    Month,
    /// 年报：按账本汇总 + 逐月走势。
    Year,
}

/// 设置页里的两个分区，焦点在菜单上时用 `↑` `↓` 切换。
///
/// 「账本」分区已经下线：账本现在就是计划（带不带上限而已），增删改名都在计划页，
/// 设置页只剩「改分类」和「修数据库」这两件真正属于设置的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Connection,
    Categories,
}

impl SettingsSection {
    /// 顺序即菜单顺序：日常记账（分类）在前，数据库运维沉到最后。
    pub const ALL: [SettingsSection; 2] = [
        SettingsSection::Categories,
        SettingsSection::Connection,
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
            SettingsSection::Connection => "运维 · 数据库",
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

/// 设置页的焦点层级：左侧分区菜单，还是右侧条目列表。
///
/// 全局约定是「`↑` `↓` 管页内、`←` `→` 翻页签」，所以设置页里换分区只能用 `↑` `↓`；
/// 分区里的账本/分类又是一份列表，为了两者不打架，把页面拆成两级：
/// 菜单里 `↑` `↓` 换分区，`Enter` 进列表，列表里 `↑` `↓` 选条目、`Esc` 退回菜单。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsFocus {
    /// 焦点在左侧分区菜单上。
    #[default]
    Menu,
    /// 焦点在右侧条目列表上（数据库连接分区没有列表，永远停在菜单）。
    Detail,
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
        Self::amount_with(label, "", hint)
    }

    /// 带初值的金额字段（编辑表单预填用）：算式求值规则和输入框完全一样。
    fn amount_with(label: &'static str, value: impl Into<String>, hint: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text,
            value: value.into(),
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

/// 计划块「不绑定分类」的选择项文案。
///
/// 单独拎成常量是因为要跟表单值、`plan_category_from_form` 的判定保持一致，
/// 改文案时不会漏掉某一处。
pub const UNBOUND_CATEGORY: &str = "(不绑定)";

/// 账单页分页结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillPage {
    /// 从 0 开始：第几页。
    pub page: usize,
    /// 总页数，空列表也算 1 页。
    pub pages: usize,
    /// 本页第一条在 [`App::bills`] 里的下标（含）。
    pub start: usize,
    /// 本页结束下标（不含）。
    pub end: usize,
    /// 每页行数预算，测试与调试用。
    pub budget: usize,
}

/// 正在新增什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    NewBill,
    NewCategory,
    /// 一个计划：名称 + 类型（收入 / 开销）+ 可选额度（计划页）。
    PlanItem,
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
    Category(u64),
    /// 某个计划账本（按 id 定位——计划页展示顺序是「先收入后开销」，和库里的 id 顺序不同）。
    PlanItem(BookId),
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
    /// 计划 / 分类都**不留空**：空值提交时会被 service 拒绝（并且提示很难懂），
    /// 所以这里直接给出一个合法的落点——
    ///
    /// * 计划：上次记账用过的那个（`preferred_book`）→ 否则「默认」→ 否则第一个；
    /// * 分类：「默认」（不存在时取第一个）；
    /// * 候选列表整个为空（刚建的库）时，塞一个同名候选，让表单仍然可用——
    ///   提交时 service 会按需自动创建这两个条目。
    ///
    /// 「日期」默认留空表示今天，需要在字段上按空格才填入上次用过的日期；
    /// 焦点默认落在「金额」——打开表单第一件事就是输钱，输完再挑分类和计划。
    /// 字段顺序也照这个动线排：金额 → 分类 → 计划 → 日期 → 备注，
    /// 因为「这笔钱花在哪」每次都要想，「记在哪个计划」基本是固定的。
    pub fn new_bill(
        book_options: Vec<String>,
        category_options: Vec<String>,
        preferred_book: Option<&str>,
    ) -> Self {
        Self::new_bill_with(book_options, category_options, preferred_book, None)
    }

    /// 带「上次用过的分类」的新增账单表单，见 [`Form::new_bill`]。
    pub fn new_bill_with(
        book_options: Vec<String>,
        category_options: Vec<String>,
        preferred_book: Option<&str>,
        preferred_category: Option<&str>,
    ) -> Self {
        let book_options = with_fallback(book_options, DEFAULT_BOOK);
        let category_options = with_fallback(category_options, DEFAULT_CATEGORY);
        let preferred_book = Some(preferred_book.unwrap_or(DEFAULT_BOOK));
        let preferred_category = Some(preferred_category.unwrap_or(DEFAULT_CATEGORY));

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
                    "分类",
                    category_options,
                    preferred_category,
                    "Enter 展开选择；新增请到「分类」页",
                ),
                picker(
                    "计划",
                    book_options,
                    preferred_book,
                    "Enter 展开选择；新增请到「计划」页",
                ),
                Field::date("日期", "留空即今天；按空格填入上次使用的时间"),
                Field::text("备注", "", "可留空"),
            ],
            focus: 1,
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
        set_field_value(&mut form.fields[2], &bill.category.name);
        set_field_value(&mut form.fields[3], &bill.book.name);
        form.fields[4].value = crate::common::time::format_datetime(bill.created_at);
        form.fields[5].value = bill.remark.clone().unwrap_or_default();
        // 账单没有自己的「计入日历」开关：跟着计划走，在计划页 / 计划详情里改
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

    /// 本月设置（概览页按 `e`）：只改起始金额。
    ///
    /// 钱怎么分已经挪到「计划」页维护，两件事各归各：
    /// 起始金额是**账上余额的起点**，计划是**这个月打算把钱分到哪几块**。
    pub fn month_settings(month: NaiveDate, current: AmountType) -> Self {
        Self {
            kind: FormKind::SetStartBalance,
            title: "本月设置（起始金额）",
            fields: vec![Field::amount_with(
                "起始金额",
                money::format_amount(current),
                "元，可写算式（+ - * / 与括号），按 = 求值",
            )],
            focus: 0,
            target: Some(EditTarget::MonthBalance(month)),
        }
    }

    /// 计划页新建 / 改一个计划。
    ///
    /// `index` 为 `None` 时是新建；为 `Some` 时预填该计划的名称、类型与额度。
    /// 提交后按账本 id 找回原记录——计划页的顺序是「先收入后开销」，
    /// 位置和库里的 id 顺序对不上，按位置找会改错人。
    ///
    /// 「额度」留空 = 不设额度：开销计划只从 0 开始累计花了多少，不判超支；
    /// 收入计划则是只累计入账、不判达标。
    /// 「计入日历」决定这个计划名下的支出进不进日历 —— 账单没有自己的开关。
    /// 账单记在哪个计划，就等于归到哪个计划——不再有「绑分类」这层映射。
    pub fn plan_item(rows: &[PlanRow], index: Option<usize>) -> Self {
        let mut form = Self {
            kind: FormKind::PlanItem,
            title: "新建计划",
            fields: vec![
                Field::text("名称", "", "例如 房租 / 吃饭 / 工资"),
                Field::choice(
                    "类型",
                    vec!["开销".to_string(), "收入".to_string()],
                    "←/→ 或空格切换：开销看上限，收入看目标",
                ),
                Field::text("额度", "", "每月额度（元）；留空就是不设额度 / 目标"),
                Field::choice(
                    "计入日历",
                    vec!["计入".to_string(), "不计入".to_string()],
                    "只影响支出日历（日额度 / 今日 / 本月已花）；账单跟着计划走",
                ),
            ],
            focus: 0,
            target: None,
        };

        if let Some(index) = index
            && let Some(row) = rows.get(index)
        {
            form.title = "编辑计划";
            form.target = Some(EditTarget::PlanItem(row.plan.id()));
            form.fields[0].value = row.name().to_string();
            form.fields[1] = Field::choice_with(
                "类型",
                vec!["开销".to_string(), "收入".to_string()],
                row.plan.kind.label().to_string(),
                "←/→ 或空格切换：开销看上限，收入看目标",
            );
            form.fields[2].value = row
                .plan
                .limit
                .map(money::format_amount)
                .unwrap_or_default();
            form.fields[3] = Field::choice_with(
                "计入日历",
                vec!["计入".to_string(), "不计入".to_string()],
                if row.plan.in_calendar {
                    "计入".to_string()
                } else {
                    "不计入".to_string()
                },
                "只影响支出日历（日额度 / 今日 / 本月已花）；账单跟着计划走",
            );
            // 名称/额度都可能改，但额度改动最多：焦点落在额度上
            form.focus = 2;
        }
        form
    }

    /// 数据库连接设置（设置页按 `e`）。
    ///
    /// 第一项是模式：本地文件 / PostgreSQL。选本地时只用到「文件路径」，
    /// 选远程时才用到下面那五个。
    pub fn connection(settings: &ConnectionSettings, sqlite_path: &str, local: bool) -> Self {
        let modes = vec!["本地文件".to_string(), "PostgreSQL".to_string()];
        let initial = if local { "本地文件" } else { "PostgreSQL" };

        let mut form = Self {
            kind: FormKind::Connection,
            title: "数据库连接",
            fields: vec![
                Field::choice_with(
                    "模式",
                    modes,
                    initial.to_string(),
                    "本地文件 = 零配置；PostgreSQL = 远程库",
                ),
                Field::text(
                    "文件路径",
                    sqlite_path.to_string(),
                    "本地库文件，目录会自动创建",
                ),
                Field::text("主机", settings.host.clone(), "IP 或主机名"),
                Field::text("端口", settings.port.to_string(), "默认 5432"),
                Field::text("用户名", settings.user.clone(), "PostgreSQL 用户"),
                Field::text("密码", settings.password.clone(), "会以明文写进 .env"),
                Field::text("数据库", settings.database.clone(), "不存在时会自动创建"),
            ],
            focus: 0,
            target: None,
        };

        form.focus = if local { 1 } else { 2 };
        form
    }

    /// 数据迁移的目标库（设置页按 `t`）。源库固定是「当前连接」，只支持远程。
    pub fn migrate_target(settings: &ConnectionSettings) -> Self {
        Self {
            kind: FormKind::MigrateData,
            title: "数据迁移（源库 = 当前连接）",
            fields: vec![
                Field::text("主机", settings.host.clone(), "IP 或主机名"),
                Field::text("端口", settings.port.to_string(), "默认 5432"),
                Field::text("用户名", settings.user.clone(), "PostgreSQL 用户"),
                Field::text("密码", settings.password.clone(), "会以明文写进 .env"),
                Field::text(
                    "数据库",
                    settings.database.clone(),
                    "目标库：会被清空后写入源库的全部数据",
                ),
            ],
            focus: 0,
            target: None,
        }
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
    /// 帮助：全量快捷键 + 几个概念的解释。`?` 打开，任意键关掉。
    Help,
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
    /// 概览页「同步今日 / 同步本周」。
    SyncRequest(SyncKind),
    /// 继续核对上次没核对完的流水（概览页按 `Enter`，或同步中途 `Esc` 后回来）。
    ResumeSync,
    Refresh,
    ShiftMonth(i32),
    /// 年报页换年。
    ShiftYear(i32),
    SubmitForm(Form),
    DeleteBill(u64),
    /// 计划页按 `d`：先查名下账单数，再弹确认（有账单时整体搬去默认计划）。
    PrepareDeletePlan(BookId),
    /// 确认后的删除；`move_bills` 为真时名下账单先整体搬到默认计划。
    DeletePlan {
        id: BookId,
        move_bills: bool,
    },
    /// 删除分类；`recursive` 为真时连同子孙一起删（终端会先提示）。
    DeleteCategory {
        id: u64,
        recursive: bool,
    },
    /// 保存连接设置并尝试立即切换（连接池由外壳重建）。
    SaveConnection(ConnectionTarget),
    /// 建库 + 建表（初始化），由外壳执行。
    InitDatabase,
    /// 把当前库的全部数据迁移到目标库，由外壳执行。
    MigrateData(ConnectionSettings),
    /// 撤销最近一次删除。
    UndoDelete,
    /// 切换一条账单是否纳入统计（账单页 `x`）。
    /// 计划页 `x`：切换这个计划是否计入日历（账单跟着计划走）。
    TogglePlanCalendar(BookId),
}

/// 设置页保存的连接目标：本地文件还是远程库。
#[derive(Debug, Clone)]
pub enum ConnectionTarget {
    /// 本地 SQLite 文件（默认，零配置）。
    Local { path: String },
    /// 远程 PostgreSQL。
    Remote(ConnectionSettings),
}

/// 一条待核对的同步流水：银行流水 + 记忆系统给出的预选建议。
#[derive(Debug, Clone)]
pub struct PendingSync {
    pub transaction: BankTransaction,
    pub suggestion: Suggestion,
}

/// 最近一次删除的内容，供 `u` 撤销。
///
/// 只保留**最后一笔**：撤销是为了救手滑，不做回收站。
#[derive(Debug, Clone)]
pub enum DeletedRecord {
    Bill(Box<BillEntity>),
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
    /// 本地模式的库文件路径（来自配置文件，设置页可改）。
    pub sqlite_path: String,
    /// 最近一次删除，按 `u` 撤销。
    pub last_deleted: Option<DeletedRecord>,
    /// 设置页当前焦点所在的分区。
    pub settings_section: SettingsSection,
    /// 设置页焦点在菜单上还是在条目列表上。
    pub settings_focus: SettingsFocus,
    /// 三个分区各自的选中行。
    settings_selected: [usize; SettingsSection::ALL.len()],
    /// 所有计划（账本 + 可选额度）在本月的执行情况，`reload` 时算一次。
    pub plans: Vec<PlanRow>,
    /// 当月额度与日历配色，在 `reload` 时算一次。
    pub progress: Option<PlanProgress>,
    /// 计划合计 vs 当月收入（盖不盖得住），同样在 `reload` 时算一次。
    pub coverage: Option<PlanCoverage>,
    /// 终端视口高度，由事件循环每帧写回。
    ///
    /// 账单页按它决定「一页放多少条」，所以渲染和翻页读的是同一份值——
    /// 否则页码和实际能看到的行数会各算各的。
    pub viewport_height: u16,
    /// 概览页正在查看的那一天（1..=当月天数），`←` `→` 移动。
    pub selected_day: u32,
    /// 还没连上数据库：界面显示配置引导页，而不是直接不给进。
    pub needs_setup: bool,
    /// 待逐条核对的银行流水（同步拉下来后排队）。
    pub pending_sync: Vec<PendingSync>,
    /// 本轮同步一共拉到多少条（用于「第 2/7 条」这类提示）。
    pub sync_total: usize,
    /// 上次记账用过的日期（跨会话记忆）；`None` 表示还没记过。
    /// 新增表单里留空即今天，按空格才把它填进去。
    pub last_entry_date: Option<NaiveDate>,
    /// 新增账单时默认选中的账本：上次记账用过的账本名（跨会话记忆）。
    pub last_entry_book: Option<String>,
    /// 上次记账用过的分类名，下次新增账单默认选中。
    pub last_entry_category: Option<String>,
    /// 每页各自记住选中行。
    /// 每个页签记住自己的选中行；末尾多一格给设置页（它不在页签队列里）。
    pub selected: [usize; Page::ALL.len() + 1],
    pub books: Vec<BillBook>,
    /// 分类按层级展开，`depth` 用于渲染缩进与父分类候选。
    pub categories: Vec<CategoryNode>,
    pub bills: Vec<BillEntity>,
    pub report: Option<MonthReport>,
    pub year_report: Option<YearReport>,
    pub modal: Option<Modal>,
    pub status: Option<Status>,
    pub should_quit: bool,
    /// 年报里空月份默认折叠成一行提示，`x` 展开成完整 12 个月。
    pub year_expanded: bool,
    /// 报表页当前看的是月报还是年报。
    pub report_tab: ReportTab,
    /// 从设置页按 `,` / `Esc` 出来时回到哪一页。
    settings_return: Page,
}

impl App {
    pub fn new(month: NaiveDate, connection_url: &str, sqlite_path: &str) -> Self {
        let today = Self::today();

        Self {
            page: Page::Overview,
            month,
            year: month.year(),
            connection: ConnectionSettings::from_url(connection_url),
            connection_url: connection_url.to_string(),
            sqlite_path: sqlite_path.to_string(),
            last_deleted: None,
            settings_section: SettingsSection::Categories,
            settings_focus: SettingsFocus::Menu,
            settings_selected: [0; SettingsSection::ALL.len()],
            plans: Vec::new(),
            progress: None,
            coverage: None,
            viewport_height: 40,
            // 看本月就默认从今天开始看，看历史月份从 1 号开始
            selected_day: if today.year() == month.year() && today.month() == month.month() {
                today.day()
            } else {
                1
            },
            needs_setup: false,
            pending_sync: Vec::new(),
            sync_total: 0,
            last_entry_date: None,
            last_entry_book: None,
            last_entry_category: None,
            selected: [0; Page::ALL.len() + 1],
            books: Vec::new(),
            categories: Vec::new(),
            bills: Vec::new(),
            report: None,
            year_report: None,
            modal: None,
            status: None,
            should_quit: false,
            year_expanded: false,
            report_tab: ReportTab::default(),
            settings_return: Page::Overview,
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
            Page::Plan => self.plans.len(),
            Page::Bills => self.bills.len(),
            // 报表页只有「月报」那一半是可选的排行列表
            Page::Reports => {
                if self.report_tab == ReportTab::Month {
                    self.month_expense_pairs().len()
                } else {
                    0
                }
            }
            Page::Overview | Page::Settings => 0,
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
            .get(self.selected_index(Page::Reports))
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
            .get(self.selected_index(Page::Reports))
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
            self.selected[self.page.slot()] = index;
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
            self.selected[page.slot()].min(count - 1)
        }
    }

    /// 账单页一页能放多少行（含日期表头）。
    ///
    /// 视口高度减去固定开销：顶栏 1 + 底栏 2 + 边框上下 2 + 页脚页码 1 = 6。
    /// 终端越高，一页看到的账单越多——不再写死 10 条把下半屏空着。
    pub fn bill_page_budget(&self) -> usize {
        usize::from(self.viewport_height).saturating_sub(6).max(2)
    }

    /// 当前选中的账单落在第几页、这一页覆盖哪几条。
    pub fn bill_page(&self) -> BillPage {
        self.bill_page_with(self.bill_page_budget())
    }

    /// 同上，但一页放多少行由调用方给。
    ///
    /// 渲染侧用**真实面板高度**算出来传进来：视口高度是渲染前一拍记下的，
    /// 窗口刚缩放的那一帧可能还对不上；以面板实际高度为准，页码才不会和内容错位。
    pub fn bill_page_with(&self, budget: usize) -> BillPage {
        self.bill_page_of(self.bills.len(), self.selected_index(Page::Bills), budget)
    }

    /// 按「整日优先、必要时才断开某一天」的贪心切页。
    ///
    /// 单位是**行**而不是账单条数：一个日期表头也占一行，所以这一天第一笔
    /// 要算 2 行。换页时该行会重新出一遍表头，翻到一半也知道自己在哪一天。
    fn bill_page_of(&self, total: usize, selected: usize, budget: usize) -> BillPage {
        let budget = budget.max(2);
        if total == 0 {
            return BillPage {
                page: 0,
                pages: 1,
                start: 0,
                end: 0,
                budget,
            };
        }

        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut start = 0usize;
        let mut used = 0usize;
        let mut index = 0usize;
        for (_, group) in crate::tui::view::bills_by_day(&self.bills) {
            for offset in 0..group.len() {
                // 一天的开始或者刚换页，都要表头
                let need = if offset == 0 || index == start { 2 } else { 1 };
                if used + need > budget && index > start {
                    ranges.push((start, index));
                    start = index;
                    used = need;
                } else {
                    used += need;
                }
                index += 1;
            }
        }
        ranges.push((start, index));

        let page = ranges
            .iter()
            .position(|(start, end)| selected >= *start && selected < *end)
            .unwrap_or(ranges.len() - 1);
        let (start, end) = ranges[page];

        BillPage {
            page,
            pages: ranges.len(),
            start,
            end,
            budget,
        }
    }

    /// PgUp / PgDn：账单页按**整页**跳，步长由视口高度决定。
    fn move_bill_page(&mut self, delta: i32) {
        let current = self.bill_page();
        let count = self.bills.len();
        if count == 0 {
            return;
        }
        let target = if delta < 0 {
            current.start.saturating_sub(1)
        } else {
            current.end.min(count.saturating_sub(1))
        };
        self.selected[Page::Bills.index()] = target;
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

    /// 翻页签。离开设置页时把两级焦点归位到菜单：
    /// 下次进来先看到「有哪几个分区」，而不是直接落在某个列表里。
    /// `,`（或老手感 `6`）进出设置页：进来记住来路，出去回到原地。
    fn toggle_settings(&mut self) -> Action {
        if self.page == Page::Settings {
            self.switch_page(self.settings_return)
        } else {
            self.settings_return = self.page;
            self.switch_page(Page::Settings)
        }
    }

    fn switch_page(&mut self, page: Page) -> Action {
        self.page = page;
        self.settings_focus = SettingsFocus::Menu;
        Action::Refresh
    }

    /// 页内上下移动：设置页要看焦点在哪一级，其余页就是选行。
    fn move_in_page(&mut self, delta: i32) {
        if self.page == Page::Settings && self.settings_focus == SettingsFocus::Menu {
            self.move_settings_section(delta);
        } else {
            self.move_selection(delta);
        }
    }

    /// 菜单态换分区。`PageUp/PageDown` 传的是 ±10，这里只当成「上一 / 下一」。
    fn move_settings_section(&mut self, delta: i32) {
        let step = delta.signum();
        if step == 0 {
            return;
        }
        let count = SettingsSection::ALL.len() as i32;
        let next = (self.settings_section.index() as i32 + step).rem_euclid(count);
        self.settings_section = SettingsSection::ALL[next as usize];
    }

    /// 跳到第一行 / 最后一行。菜单态下没有行可选，直接忽略。
    fn jump_in_page(&mut self, index: usize) {
        if self.page == Page::Settings && self.settings_focus == SettingsFocus::Menu {
            return;
        }
        self.jump_selection(index);
    }

    fn clamp_selection(&mut self) {
        for page in Page::ALL {
            let count = self.row_count(page);
            if count == 0 {
                self.selected[page.slot()] = 0;
            } else {
                self.selected[page.slot()] = self.selected[page.slot()].min(count - 1);
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
            options.push(format!("{}{}", "  ".repeat(node.depth), node.category.name));
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
                SettingsSection::Connection => Form::connection(
                    &self.connection,
                    &self.sqlite_path,
                    self.connection_url.starts_with("sqlite:"),
                ),
                SettingsSection::Categories => Form::new_category(self.parent_options(None)),
            },
            // 计划页按 `a` 新增一块资金
            Page::Plan => Form::plan_item(&self.plans, None),
            // 概览/账单/报表都认为你要记账
            Page::Overview | Page::Bills | Page::Reports => Form::new_bill_with(
                self.book_names(),
                self.category_names(),
                self.last_entry_book.as_deref(),
                self.last_entry_category.as_deref(),
            ),
        };
        self.modal = Some(Modal::Form(form));
    }

    /// 记完一笔后的收尾：留在当前页签，只把概览页的选中日期挪到刚记的那一天。
    ///
    /// 页签**不动**：人在账单页连着记几笔时，不该记一笔就被踹回概览。
    /// 「高亮新行」在概览里靠日历格子配色体现——选中日期就是那笔账的日子，
    /// 人回到概览一眼就能看到它是插进哪一格的（跨月的账不挪，免得月份跳来跳去）。
    fn focus_new_bill_day(&mut self) {
        let latest = self
            .bills
            .iter()
            .max_by_key(|bill| bill.id)
            .map(|bill| bill.created_at.date_naive());

        if let Some(date) = latest {
            if date.year() == self.month.year() && date.month() == self.month.month() {
                self.selected_day = date.day();
            }
        }
    }

    /// 设置页按 `t`：打开「数据迁移」表单（目标库默认预填当前连接）。
    fn open_migrate_form(&mut self) {
        self.modal = Some(Modal::Form(Form::migrate_target(&self.connection)));
    }

    fn open_edit_form(&mut self) {
        let form = match self.page {
            // 概览 / 报表的「可编辑内容」是本月起始金额
            Page::Overview | Page::Reports => {
                let current = self
                    .report
                    .as_ref()
                    .map(|report| report.collection.start_balance)
                    .unwrap_or(0);
                Some(Form::month_settings(self.month, current))
            }
            // 计划页按 `e`：改选中的那一块
            Page::Plan => {
                let index = self.selected_index(Page::Plan);
                self.plans
                    .get(index)
                    .map(|_| Form::plan_item(&self.plans, Some(index)))
            }
            // 设置页按 `e`：改的是当前分区里的东西
            Page::Settings => match self.settings_section {
                SettingsSection::Connection => Some(Form::connection(
                    &self.connection,
                    &self.sqlite_path,
                    self.connection_url.starts_with("sqlite:"),
                )),
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
            // 计划页走 Action::PrepareDeletePlan：要先查名下的账单数
            Page::Plan => None,
            Page::Settings => match self.settings_section {
                // 连接分区没有可删的东西
                SettingsSection::Connection => None,
                SettingsSection::Categories => self.category_delete_request(),
            },
            // 概览/报表没有可删的东西（设置页已经在上面按分区处理了）
            Page::Overview | Page::Reports => None,
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

    /// `[` / `]` 的含义随页面变化：报表页的「年报」那一半换年，其余换月。
    fn shift_period(&self, delta: i32) -> Action {
        if self.page == Page::Reports && self.report_tab == ReportTab::Year {
            Action::ShiftYear(delta)
        } else {
            Action::ShiftMonth(delta)
        }
    }

    /// 处理一次按键，返回外壳需要执行的动作。
    pub fn on_key(&mut self, key: Key) -> Action {
        // 弹窗优先：表单里的 s/w 是普通字符，不能被下面的快捷键吃掉
        if self.modal.is_some() {
            return self.on_key_modal(key);
        }

        // 还没连上数据库：只放行「切页 / 退出」。否则用户对着表单填完才发现写不进去。
        if self.needs_setup {
            match key {
                Key::Char('1'..='6') | Key::Char(',') | Key::Tab | Key::BackTab
                | Key::Left | Key::Right => {}
                Key::Char('?') => {}
                Key::Ctrl('c') | Key::Char('q') => {}
                _ => return Action::None,
            }
        }

        // 概览页那两个「同步」按钮的快捷键：s = 今日、w = 本周。
        // 只在概览页生效——在别的页面按一下 s 就开始连银行，那是惊吓不是快捷。
        if self.page == Page::Overview {
            match key {
                Key::Char('s') => return Action::SyncRequest(SyncKind::Today),
                Key::Char('w') => return Action::SyncRequest(SyncKind::Week),
                _ => {}
            }
        }

        // Ctrl+C / q 退出
        if key == Key::Ctrl('c') || key == Key::Char('q') {
            self.should_quit = true;
            return Action::None;
        }

        match key {
            // 设置不进页签队列：`,` 进出，`6` 留给老手感
            Key::Char(',') | Key::Char('6') => self.toggle_settings(),
            Key::Char('1'..='6') => {
                if let Key::Char(digit) = key {
                    let index = digit as usize - '1' as usize;
                    if let Some(page) = Page::ALL.get(index) {
                        return self.switch_page(*page);
                    }
                }
                Action::None
            }
            Key::Tab => self.switch_page(self.page.next()),
            Key::BackTab => self.switch_page(self.page.previous()),
            Key::Up => {
                if self.page == Page::Overview {
                    // 概览页用 ↑↓ 换查看的日期
                    self.move_inspected_day(-1);
                    Action::Refresh
                } else {
                    self.move_in_page(-1);
                    Action::None
                }
            }
            Key::Down => {
                if self.page == Page::Overview {
                    self.move_inspected_day(1);
                    Action::Refresh
                } else {
                    self.move_in_page(1);
                    Action::None
                }
            }
            Key::PageUp => {
                // 概览页往前一周；账单页按整页翻；其它列表页往上 10 行
                if self.page == Page::Overview {
                    self.move_inspected_day(-7);
                    Action::Refresh
                } else if self.page == Page::Bills {
                    self.move_bill_page(-1);
                    Action::None
                } else {
                    self.move_in_page(-10);
                    Action::None
                }
            }
            Key::PageDown => {
                if self.page == Page::Overview {
                    self.move_inspected_day(7);
                    Action::Refresh
                } else if self.page == Page::Bills {
                    self.move_bill_page(1);
                    Action::None
                } else {
                    self.move_in_page(10);
                    Action::None
                }
            }
            Key::Home => {
                if self.page == Page::Overview {
                    self.selected_day = 1;
                    Action::Refresh
                } else {
                    self.jump_in_page(0);
                    Action::None
                }
            }
            Key::End => {
                if self.page == Page::Overview {
                    self.selected_day = days_in_month(self.month).max(1);
                    Action::Refresh
                } else {
                    let count = self.current_row_count();
                    self.jump_in_page(count.saturating_sub(1));
                    Action::None
                }
            }
            // ←/→ 永远翻页签（设置页也一样）。页签是横向维度、页内一律交给 ↑↓，
            // 两套方向键各管一维，就不会出现「同一个键在这个页面干另一件事」。
            Key::Left => self.switch_page(self.page.previous()),
            Key::Right => self.switch_page(self.page.next()),
            // 设置页没有「月份」这个概念，`[` `]` 不做事，免得误以为换了月份
            Key::Char('[') => {
                if self.page == Page::Settings {
                    Action::None
                } else {
                    self.shift_period(-1)
                }
            }
            Key::Char(']') => {
                if self.page == Page::Settings {
                    Action::None
                } else {
                    self.shift_period(1)
                }
            }
            // 设置页：Enter 进列表 / 在列表里编辑；概览页：继续核对没核完的流水
            Key::Enter => {
                if self.page == Page::Settings {
                    match self.settings_focus {
                        SettingsFocus::Menu => {
                            // 数据库连接分区没有列表，Enter 就没什么可进的
                            if self.settings_row_count() > 0 {
                                self.settings_focus = SettingsFocus::Detail;
                            }
                            Action::None
                        }
                        SettingsFocus::Detail => {
                            self.open_edit_form();
                            Action::None
                        }
                    }
                } else if self.page == Page::Overview && !self.pending_sync.is_empty() {
                    Action::ResumeSync
                } else if self.page == Page::Bills || self.page == Page::Plan {
                    // 列表里 Enter = 打开选中那条，和大多数终端工具一致
                    self.open_edit_form();
                    Action::None
                } else {
                    Action::None
                }
            }
            // 设置页：列表里 Esc 退回菜单，菜单里 Esc 直接退出设置页
            // （其余情况 Esc 不做事，弹窗的 Esc 在 on_key_modal 里）
            Key::Esc => {
                if self.page == Page::Settings {
                    match self.settings_focus {
                        SettingsFocus::Detail => self.settings_focus = SettingsFocus::Menu,
                        SettingsFocus::Menu => self.page = self.settings_return,
                    }
                }
                Action::None
            }
            Key::Char('r') => Action::Refresh,
            // 帮助是全局的：不占版面，按一下才出现
            Key::Char('?') => {
                self.modal = Some(Modal::Help);
                Action::None
            }
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
                // 报表页的「年报」：空月份默认折叠，x 展开 / 收起
                if self.page == Page::Reports && self.report_tab == ReportTab::Year {
                    self.year_expanded = !self.year_expanded;
                    Action::None
                // 计划页：切这个计划进不进日历；账单没有自己的开关
                } else if self.page == Page::Plan {
                    self.plans
                        .get(self.selected_index(Page::Plan))
                        .map(|row| Action::TogglePlanCalendar(row.plan.id()))
                        .unwrap_or(Action::None)
                } else {
                    Action::None
                }
            }
            Key::Char('d') | Key::Delete => {
                if self.page == Page::Plan {
                    // 计划带账单时得先查数、再决定要不要搬，不能同步弹框
                    match self.plans.get(self.selected_index(Page::Plan)) {
                        Some(row) => Action::PrepareDeletePlan(row.plan.id()),
                        None => {
                            self.set_info("当前页没有可删除的内容");
                            Action::None
                        }
                    }
                } else {
                    self.request_delete();
                    Action::None
                }
            }
            // 报表页按 y：切到年报（此时 `[` `]` 换的是年份）
            Key::Char('y') => {
                if self.page == Page::Reports {
                    self.report_tab = ReportTab::Year;
                    Action::Refresh
                } else {
                    Action::None
                }
            }
            Key::Char('m') => {
                let today = Self::today();
                // 报表页按 m：先切回月报，再回到本月
                if self.page == Page::Reports {
                    self.report_tab = ReportTab::Month;
                }
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
            Modal::Notice { .. } => {
                // 提示框不拦人：任意键（含 Esc / Enter）关掉，别让用户猜哪个键能用
                Action::None
            }
            // 帮助也一样：看一眼就走，任意键关掉
            Modal::Help => Action::None,
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
                                // 迁移只支持远程库：本地模式只在「数据库连接」里可选
                                Ok(ConnectionTarget::Local { path })
                                    if form.kind == FormKind::Connection =>
                                {
                                    keep_open = false;
                                    action =
                                        Action::SaveConnection(ConnectionTarget::Local { path });
                                }
                                Ok(ConnectionTarget::Remote(settings)) => {
                                    keep_open = false;
                                    action = if form.kind == FormKind::Connection {
                                        Action::SaveConnection(ConnectionTarget::Remote(settings))
                                    } else {
                                        Action::MigrateData(settings)
                                    };
                                }
                                Ok(ConnectionTarget::Local { .. }) => {
                                    keep_open = true;
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
            Action::SyncRequest(kind) => {
                self.sync_from_bank(kind, services).await?;
            }
            Action::ResumeSync => {
                // 概览页提示「待核对 N 条 · 按 Enter 继续」对应的落地动作
                if self.pending_sync.is_empty() {
                    self.set_info("没有待核对的流水");
                } else {
                    self.open_next_reconcile(services);
                }
            }
            Action::SubmitForm(form) => {
                // 顺手记的一笔：存完留在当前页签，只把概览的选中日期挪到刚记的那天
                let quick_entry = form.kind == FormKind::NewBill && self.pending_sync.is_empty();

                if !self.pending_sync.is_empty() && form.kind == FormKind::NewBill {
                    // 核对流程里，提交表单就是「确认这一条」
                    self.confirm_reconcile(&form, services).await?;
                } else {
                    let message = submit(form, services).await?;
                    self.set_info(message);
                }
                self.reload(services).await?;

                if quick_entry {
                    self.focus_new_bill_day();
                }
            }
            Action::DeleteBill(id) => {
                // 删之前先把整条记下来，`u` 才有东西可恢复
                let record = self.bill_record(id);

                services.bills.remove(id).await?;
                self.last_deleted = record;
                self.set_info(format!("已删除账单 #{id}（按 u 撤销）"));
                self.reload(services).await?;
            }
            Action::PrepareDeletePlan(id) => {
                let plan = services.plans.by_id(id).await?;
                let bills = services.plans.bill_count(id).await?;

                if bills > 0 && plan.name() == DEFAULT_BOOK {
                    self.modal = Some(Modal::Notice {
                        title: "不能删除".to_string(),
                        message: format!(
                            "「{}」是默认计划，名下 {} 笔账单没处可移，先把它们改到别的计划再删。",
                            plan.name(),
                            bills
                        ),
                    });
                } else {
                    let message = if bills > 0 {
                        format!(
                            "计划「{}」（{}）下有 {} 笔账单，删除后全部移到「{}」计划，收支归属随之改变。此操作不可撤销。",
                            plan.name(),
                            plan.describe_limit(),
                            bills,
                            DEFAULT_BOOK
                        )
                    } else {
                        format!(
                            "确定要删除计划「{}」（{}）吗？此操作不可撤销。",
                            plan.name(),
                            plan.describe_limit()
                        )
                    };
                    self.modal = Some(Modal::Confirm {
                        title: "确认删除".to_string(),
                        message,
                        action: Action::DeletePlan {
                            id,
                            move_bills: bills > 0,
                        },
                    });
                }
            }
            Action::DeletePlan { id, move_bills } => {
                if move_bills {
                    let (plan, moved) =
                        services.plans.remove_moving_bills(&id.to_string(), DEFAULT_BOOK).await?;
                    self.set_info(format!(
                        "已删除计划「{}」，{} 笔账单移到「{}」",
                        plan.name(),
                        moved,
                        DEFAULT_BOOK
                    ));
                } else {
                    let plan = services.plans.remove(&id.to_string()).await?;
                    self.set_info(format!("已删除计划「{}」", plan.name()));
                }
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
            Action::TogglePlanCalendar(id) => {
                let next = self
                    .plans
                    .iter()
                    .find(|row| row.plan.id() == id)
                    .map(|row| !row.plan.in_calendar);

                match next {
                    None => self.set_error(format!("计划 id={id} 不在当前列表里")),
                    Some(next) => {
                        let plan = services.plans.set_in_calendar(&id.to_string(), next).await?;
                        self.reload(services).await?;
                        self.set_info(if next {
                            format!("计划「{}」已恢复计入日历", plan.name())
                        } else {
                            format!("计划「{}」已标为「不计入日历」", plan.name())
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

    /// 从银行拉流水 → 去重 → 逐条核对（复用「新增账单」表单预填）。
    async fn sync_from_bank(&mut self, kind: SyncKind, services: &Services) -> Result<()> {
        let bank = BankService::new(services.pool().clone());
        let today = BankService::today();

        let Some(window) = bank.window(kind, today).await? else {
            self.set_info("本周本地已经记到最新，没有需要同步的日子。");
            return Ok(());
        };

        self.set_info(format!(
            "正在同步{}流水（{}）：启动/连接浏览器并监听会话…",
            window.label,
            window.describe()
        ));

        let session = crate::bank::sync::ensure_session(crate::bank::browser::DEFAULT_PORT).await?;

        // 账户优先用监听到的；监听不到才用数据库里保存的兜底
        let account = match session.account() {
            Some(account) => account,
            None => match crate::bank::sync::stored_account(&bank).await? {
                Some(account) => account,
                None => {
                    self.set_error(
                        "没从页面请求里读到账户信息：请在弹出的窗口里登录招商银行网银并刷新一次",
                    );
                    return Ok(());
                }
            },
        };

        let fetched = crate::bank::sync::fetch_with(&session, &account, &window).await?;
        let fresh = bank.new_transactions(fetched).await?;

        if fresh.is_empty() {
            self.set_info(format!("{}流水都已导入过，没有新的。", window.label));
            return Ok(());
        }

        let mut pending = Vec::with_capacity(fresh.len());
        for transaction in fresh {
            let suggestion = bank.suggest(&transaction.memo).await?;
            pending.push(PendingSync {
                transaction,
                suggestion,
            });
        }

        self.sync_total = pending.len();
        self.pending_sync = pending;
        self.set_info(format!(
            "{}：拉到 {} 条新流水，逐条核对（Esc 放弃剩余）",
            window.label, self.sync_total
        ));

        self.open_next_reconcile(services);
        Ok(())
    }

    /// 打开下一条待核对流水；队列空了就收尾。
    fn open_next_reconcile(&mut self, services: &Services) {
        let Some(item) = self.pending_sync.first().cloned() else {
            self.modal = None;
            self.set_info("核对完成，全部导入。");
            return;
        };

        let books: Vec<String> = self.books.iter().map(|book| book.name.clone()).collect();
        let categories: Vec<String> = self
            .categories
            .iter()
            .map(|node| node.category.name.clone())
            .collect();

        let mut form = Form::new_bill(books, categories, Some(item.suggestion.book.as_str()));
        set_field(
            &mut form,
            "方向",
            match item.transaction.kind {
                BillKind::Income => "收入",
                BillKind::Expense => "支出",
            },
        );
        set_field(&mut form, "金额", &yuan_text(item.transaction.amount));
        set_field(&mut form, "分类", &item.suggestion.category);
        set_field(&mut form, "日期", &item.transaction.date().to_string());
        set_field(&mut form, "备注", &item.transaction.memo);

        let index = self.sync_total.saturating_sub(self.pending_sync.len()) + 1;
        let _ = services;
        self.set_info(format!(
            "核对 {index}/{}：{}　建议来源：{}",
            self.sync_total,
            item.transaction.describe(),
            item.suggestion.source.describe()
        ));
        self.modal = Some(Modal::Form(form));
    }

    /// 确认核对队列里的第一条（按表单里可能改过的方向/金额/日期/备注入账）。
    async fn confirm_reconcile(&mut self, form: &Form, services: &Services) -> Result<()> {
        let Some(item) = self.pending_sync.first().cloned() else {
            return Ok(());
        };

        let mut transaction = item.transaction.clone();
        transaction.kind = if form.value("方向") == "收入" {
            BillKind::Income
        } else {
            BillKind::Expense
        };
        if let Ok(amount) = money::parse_amount(form.value("金额")) {
            transaction.amount = amount;
        }
        if let Ok(date) = NaiveDate::parse_from_str(form.value("日期"), "%Y-%m-%d") {
            transaction.date_time = date.and_time(transaction.date_time.time());
        }
        let memo = form.value("备注");
        if !memo.is_empty() {
            transaction.memo = memo.to_string();
        }

        let bank = BankService::new(services.pool().clone());
        let bill = bank
            .confirm(&transaction, form.value("计划"), form.value("分类"))
            .await?;

        self.pending_sync.remove(0);
        self.set_info(format!(
            "已导入 {} {} → {} / {}",
            bill.created_at.format("%Y-%m-%d"),
            yuan_text(bill.amount),
            bill.book.name,
            bill.category.name
        ));

        self.open_next_reconcile(services);
        Ok(())
    }

    /// 没有可用连接时进入引导状态：清空数据并标记，让界面显示配置引导页。
    ///
    /// 关键是**仍然把 UI 交给用户**——不然连不上库就进不去，连改设置的入口都没有。
    pub fn mark_needs_setup(&mut self) {
        self.needs_setup = true;
        // 连不上库时唯一能干的事就是配连接，别让用户先去翻菜单
        self.settings_section = SettingsSection::Connection;
        self.settings_focus = SettingsFocus::Menu;
        self.books.clear();
        self.categories.clear();
        self.bills.clear();
        self.report = None;
        self.year_report = None;
        self.progress = None;
        self.coverage = None;
        self.plans.clear();
        self.modal = None;
    }

    /// 重新读取当前页需要的数据。
    pub async fn reload(&mut self, services: &Services) -> Result<()> {
        // 能读到数据就说明连接是通的，引导页可以收起来了
        self.needs_setup = false;

        self.books = services.books.list().await?;
        self.categories = services.categories.tree().await?;
        self.bills = services.bills.list_in_month(self.month).await?;
        self.report = Some(services.months.report(self.month).await?);
        self.year_report = Some(services.months.year_report(self.year).await?);
        self.last_entry_date = services.bills.last_used_date().await?;
        self.last_entry_book = services.bills.default_entry_book().await?;
        self.last_entry_category = services.bills.default_entry_category().await?;

        // 计划是全局的，本月已花与日历按当前查看的月份算
        let today = if self.viewing_current_month() {
            Some(Self::today())
        } else {
            None
        };
        let snapshot = services.plans.snapshot(self.month, today).await?;
        self.plans = snapshot.rows;
        self.progress = Some(snapshot.progress);
        self.coverage = Some(snapshot.coverage);
        self.clamp_selection();
        Ok(())
    }
}

/// 提交表单，返回给状态栏的提示语。
async fn submit(form: Form, services: &Services) -> Result<String> {
    match (form.kind, form.target) {
        (FormKind::NewBill, Some(EditTarget::Bill(id))) => {
            let bill = services.bills.update(id, bill_from_form(&form)?).await?;

            Ok(format!(
                "已更新账单 #{} 为 {} {} 元",
                bill.id,
                bill.kind,
                money::format_amount(bill.amount)
            ))
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
            // 和账单金额同一条路：纯数字照旧，带运算符就先求值
            let amount = amount_from_field(form.value("起始金额"))?;

            services.months.set_start_balance(month, amount).await?;

            Ok(format!(
                "{} 起始金额 {} 元",
                crate::common::time::format_month(month),
                money::format_amount(amount)
            ))
        }
        (FormKind::PlanItem, Some(EditTarget::PlanItem(id))) => {
            let name = plan_name(&form)?;
            let limit = plan_limit_from_form(&form)?;
            let kind = plan_kind_from_form(&form);
            let in_calendar = plan_calendar_from_form(&form);

            // 按 id 取原记录：列表顺序是「先收入后开销」，与库里顺序不同，不能按位置找
            let current = services.plans.by_id(id).await?;
            if current.name() != name {
                services.plans.rename(current.name(), &name).await?;
            }
            services.plans.set_limit(&name, limit).await?;
            services.plans.set_kind(&name, kind).await?;
            let plan = services.plans.set_in_calendar(&name, in_calendar).await?;

            Ok(format!(
                "计划「{}」已改为{}计划 · {}",
                plan.name(),
                plan.kind.label(),
                plan.describe_limit()
            ))
        }
        (FormKind::PlanItem, None) => {
            let name = plan_name(&form)?;
            let limit = plan_limit_from_form(&form)?;
            let kind = plan_kind_from_form(&form);
            let in_calendar = plan_calendar_from_form(&form);
            let plan = services.plans.add(&name, limit, kind).await?;
            // 新建默认计入日历；只有明确选「不计入」才多写一次
            let plan = if in_calendar {
                plan
            } else {
                services.plans.set_in_calendar(&name, false).await?
            };

            Ok(format!(
                "已新增{}计划「{}」 · {}{}",
                plan.kind.label(),
                plan.name(),
                plan.describe_limit(),
                if plan.in_calendar { "" } else { " · 不计入日历" }
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
/// `preferred` 是上次用过的值，存在于候选里时作为默认选中项；
/// 不存在（比如上次用的账本已经被删掉）则回落到第一个候选，
/// 不会把表单留成空值 —— 空值提交只会换来一句「账本名称不能为空」。
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

/// 候选列表为空时补一个兜底候选。
///
/// 刚建好的库里没有任何账本 / 分类，表单如果只给一个空文本框，
/// 用户很容易直接回车提交，然后被 service 以「名称不能为空」打回来。
/// 塞一个默认名称进去，提交时 service 会按需自动创建。
fn with_fallback(options: Vec<String>, fallback: &str) -> Vec<String> {
    if options.is_empty() {
        vec![fallback.to_string()]
    } else {
        options
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

/// 计划名：不允许为空——计划靠名字找，空名等于没建。
fn plan_name(form: &Form) -> Result<String> {
    let name = form.value("名称");
    if name.is_empty() {
        bail!("计划名不能为空");
    }
    Ok(name.to_string())
}

/// 计划是否计入日历：表单里选「计入 / 不计入」，默认计入。
fn plan_calendar_from_form(form: &Form) -> bool {
    form.value("计入日历") != "不计入"
}

/// 计划类型：表单里选的是「开销 / 收入」，落库用 `PlanKind`。
fn plan_kind_from_form(form: &Form) -> PlanKind {
    match form.value("类型") {
        "收入" => PlanKind::Income,
        _ => PlanKind::Expense,
    }
}

/// 计划额度：留空就是不设额度。和账单金额一样支持算式。
fn plan_limit_from_form(form: &Form) -> Result<Option<AmountType>> {
    let raw = form.value("额度");
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let text = if raw.chars().any(|character| "+-*/()".contains(character)) {
        money::evaluate_expression(raw).map_err(|error| anyhow!("{error}"))?
    } else {
        raw.to_string()
    };
    let amount = money::parse_amount(&text).map_err(|error| anyhow!("{error}"))?;
    if amount <= 0 {
        bail!("额度要大于 0；不想设额度就把「额度」留空");
    }
    Ok(Some(amount))
}

/// 金额输入框 → 分：带运算符先求值（等价于按过 `=`），纯数字保留两位小数校验。
///
/// 账单金额和本月起始金额共用这一条路，别让两个输入框的算法跑偏。
fn amount_from_field(raw: &str) -> Result<AmountType> {
    let text = if raw.chars().any(|character| "+-*/()".contains(character)) {
        money::evaluate_expression(raw).map_err(|error| anyhow!("{error}"))?
    } else {
        raw.to_string()
    };
    money::parse_amount(&text).map_err(|error| anyhow!("{error}"))
}

/// 从表单构造账单入参。
fn bill_from_form(form: &Form) -> Result<NewBill> {
    let kind = match form.value("方向") {
        "收入" => BillKind::Income,
        _ => BillKind::Expense,
    };
    // 直接提交算式也等价于按过 `=`；纯数字仍走原解析，保留「最多两位小数」的校验
    let amount = amount_from_field(form.value("金额"))?;

    let date = form.value("日期");
    let created_at = if date.is_empty() {
        Utc::now()
    } else {
        crate::common::time::parse_datetime(Some(date)).map_err(|error| anyhow!("{error}"))?
    };

    Ok(NewBill {
        kind,
        amount,
        book: require(form.value("计划"), "计划名称不能为空")?.to_string(),
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
fn connection_from_form(form: &Form) -> Result<ConnectionTarget> {
    // 第一项是模式；选本地时只读文件路径
    if form.value("模式") == "本地文件" {
        let path = require(form.value("文件路径"), "本地库文件路径不能为空")?.to_string();
        return Ok(ConnectionTarget::Local { path });
    }

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

    Ok(ConnectionTarget::Remote(ConnectionSettings {
        host,
        port,
        user,
        password: form.value("密码").to_string(),
        database,
    }))
}

/// 往表单的某个字段里填值（按标签找）。
fn set_field(form: &mut Form, label: &str, value: &str) {
    if let Some(field) = form.fields.iter_mut().find(|field| field.label == label) {
        field.value = value.to_string();
    }
}

/// 分 → `12.34`（不带千分位：金额字段的解析器不认逗号）。
fn yuan_text(cents: AmountType) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let absolute = cents.abs();
    format!("{sign}{}.{:02}", absolute / 100, absolute % 100)
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
    use crate::bill_book::model::{Plan, PlanKind};
    use crate::plan::model::PlanRow;

    /// 测试用连接串：各字段值都便于断言。
    const TEST_CONNECTION_URL: &str = "postgres://postgres:postgres@127.0.0.1:5432/biller";
    const TEST_SQLITE_PATH: &str = "C:/Users/test/.biller/biller.db";

    /// 造一个计划：测试里只关心名字和额度，账本 id 随便给。
    fn plan_row(name: &str, limit: Option<AmountType>) -> Plan {
        Plan::new(
            BillBook {
                id: 1,
                name: name.into(),
            },
            limit,
            PlanKind::Expense,
        )
    }

    fn sample_app() -> App {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut app = App::new(month, TEST_CONNECTION_URL, TEST_SQLITE_PATH);

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
        assert_eq!(app.page, Page::Plan);
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
        app.on_key(Key::Char('3'));
        assert_eq!(app.page, Page::Bills, "第 3 页是账单");
        app.on_key(Key::Char('4'));
        assert_eq!(app.page, Page::Reports, "第 4 页是报表（月报 + 年报）");
        app.on_key(Key::Char('5'));
        assert_eq!(app.page, Page::Reports, "越界数字应被忽略");

        app.on_key(Key::Char('6'));
        assert_eq!(app.page, Page::Settings, "6 是老手感，仍可进设置");
        app.on_key(Key::Char('6'));
        assert_eq!(app.page, Page::Reports, "再按一次回到刚才那一页");
        app.on_key(Key::Char(','));
        assert_eq!(app.page, Page::Settings, ", 是设置页的新入口");
        app.on_key(Key::Esc);
        assert_eq!(app.page, Page::Reports, "Esc 出设置页，回到来路");

        app.on_key(Key::Char('9'));
        assert_eq!(app.page, Page::Reports, "越界数字应被忽略");
    }

    #[test]
    fn reports_page_switches_between_month_and_year() {
        let mut app = sample_app();
        assert_eq!(app.page, Page::Overview);

        app.on_key(Key::Char('4'));
        assert_eq!(app.page, Page::Reports);
        assert_eq!(app.report_tab, ReportTab::Month, "报表页默认看月报");

        app.on_key(Key::Char('y'));
        assert_eq!(app.report_tab, ReportTab::Year, "y 切年报");
        assert_eq!(app.page, Page::Reports, "切时间尺度不该换页");

        app.on_key(Key::Char('m'));
        assert_eq!(app.report_tab, ReportTab::Month, "m 切回月报");
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
        assert_eq!(app.page, Page::Plan);
        app.on_key(Key::Right);
        assert_eq!(app.page, Page::Bills);
        app.on_key(Key::Left);
        assert_eq!(app.page, Page::Plan);
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
                assert_eq!(form.value("计划"), "微信");
                assert_eq!(form.value("分类"), "餐饮");
            }
            other => panic!("应打开新增账单表单，实际 {other:?}"),
        }
    }

    #[test]
    fn bill_form_keeps_a_usable_default_on_an_empty_database() {
        let mut app = App::new(
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            TEST_CONNECTION_URL,
            TEST_SQLITE_PATH,
        );
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        match &app.modal {
            Some(Modal::Form(form)) => {
                // 空库也不能留空值：空着提交只会被 service 打回「名称不能为空」，
                // 所以退化成「只有一个默认候选」的选择框，提交时自动创建
                assert!(form.fields[2].is_choice(), "账本应给出默认候选");
                assert_eq!(form.value("计划"), DEFAULT_BOOK);
                assert!(form.fields[3].is_choice(), "分类应给出默认候选");
                assert_eq!(form.value("分类"), DEFAULT_CATEGORY);
            }
            other => panic!("应打开新增账单表单，实际 {other:?}"),
        }
    }

    #[test]
    fn new_bill_form_focuses_amount_and_leaves_date_empty() {
        let form = Form::new_bill(vec!["微信".to_string()], vec!["餐饮".to_string()], None);

        assert_eq!(form.focus, 1, "焦点应默认落在「金额」");
        assert_eq!(form.fields[1].label, "金额");
        assert_eq!(form.value("计划"), "微信");
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
        assert_eq!(form.value("计划"), "支付宝");

        // 上次那本账本已被删除 → 回落到第一个
        let form = Form::new_bill(books, categories, Some("已删除的账本"));
        assert_eq!(form.value("计划"), "微信");
    }

    #[test]
    fn new_bill_form_prefers_last_used_category() {
        let books = vec!["微信".to_string()];
        let categories = vec!["餐饮".to_string(), "交通".to_string()];

        // 上次记的是「交通」→ 默认选中它
        let form = Form::new_bill_with(books.clone(), categories.clone(), None, Some("交通"));
        assert_eq!(form.value("分类"), "交通");

        // 上次那个分类已被删除 → 回落到列表第一个
        let form = Form::new_bill_with(books, categories, None, Some("已删除的分类"));
        assert_eq!(form.value("分类"), "餐饮");
    }

    #[test]
    fn bill_form_asks_category_before_plan() {
        let form = Form::new_bill(vec!["微信".to_string()], vec!["餐饮".to_string()], None);

        // 记账动线：金额 → 分类（每次都变）→ 计划（基本固定）
        assert_eq!(form.fields[1].label, "金额");
        assert_eq!(form.fields[2].label, "分类");
        assert_eq!(form.fields[3].label, "计划");
    }

    #[test]
    fn quick_entry_stays_on_current_page() {
        // 四个页签都能记一笔：记完原地不动，只把概览的选中日期挪到新账单那天
        for page in Page::ALL {
            let mut app = sample_app();
            app.page = page;
            app.selected_day = 1;
            let mut bill = sample_bill(9, 1234);
            // 固定日期，别让断言跟着系统时间漂
            bill.created_at = NaiveDate::from_ymd_opt(2026, 10, 12)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc();
            app.bills = vec![bill];

            app.focus_new_bill_day();

            assert_eq!(app.page, page, "记完一笔应留在当前页签「{}」", page.title());
            assert_eq!(
                app.selected_day, 12,
                "概览的选中日期应挪到刚记的那一天，日历上就能看到它"
            );
        }
    }

    #[test]
    fn quick_entry_keeps_month_when_bill_is_elsewhere() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.selected_day = 3;
        app.bills = vec![];
        app.month = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();

        app.focus_new_bill_day();

        assert_eq!(app.page, Page::Bills, "跨月的账也不该换页签");
        assert_eq!(app.selected_day, 3, "查不到本月的账时不动选中日期");
    }

    #[test]
    fn space_on_empty_date_field_fills_last_used_date() {
        let mut app = sample_app();
        app.last_entry_date = Some(NaiveDate::from_ymd_opt(2026, 9, 30).unwrap());
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        // 焦点：金额(1) → 分类(2) → 账本(3) → 日期(4)
        app.on_key(Key::Tab);
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
            Some(Modal::Confirm {
                message, action, ..
            }) => {
                assert!(message.contains("餐饮"), "{message}");
                assert!(
                    message.contains("子分类"),
                    "应提示会连带删除子分类：{message}"
                );
                assert!(message.contains("午餐"), "应列出子分类名称：{message}");
                assert!(
                    matches!(
                        action,
                        Action::DeleteCategory {
                            recursive: true,
                            ..
                        }
                    ),
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
            Some(Modal::Confirm {
                message, action, ..
            }) => {
                assert!(message.contains("午餐"), "{message}");
                assert!(matches!(
                    action,
                    Action::DeleteCategory {
                        recursive: false,
                        ..
                    }
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

        // 焦点默认就在「金额」，打开表单直接输钱
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
        // 焦点默认在「金额」，退一格到「方向」再切换
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
        // 焦点默认在「金额」文本框上，回车就是提交
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
        app.bills = vec![sample_bill(7, 1000), sample_bill(9, 2000)];
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
        app.settings_section = SettingsSection::Categories;
        // 先按 Enter 进列表：↑↓ 在菜单态是换分区，不换条目
        app.settings_focus = SettingsFocus::Detail;
        app.categories.truncate(2);
        app.on_key(Key::Up);
        assert_eq!(app.settings_index(), 1, "向上应回绕到末行");
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 0, "向下应回绕到首行");

        app.categories.truncate(1);
        app.clamp_selection();
        assert_eq!(app.settings_index(), 0);
    }

    #[test]
    fn a_key_opens_page_specific_form() {
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
    fn left_right_always_switch_pages() {
        let mut app = sample_app();
        app.page = Page::Settings;

        // 全局约定：← → 只翻页签；设置不在页签队列里，从它出发落到队列两端
        assert!(matches!(app.on_key(Key::Right), Action::Refresh));
        assert_eq!(app.page, Page::Overview, "设置在队列外，→ 落到第一页");
        app.on_key(Key::Left);
        assert_eq!(app.page, Page::Reports, "← 从概览回绕到最后一页");

        // Tab / BackTab 同义
        app.page = Page::Settings;
        app.on_key(Key::Tab);
        assert_eq!(app.page, Page::Overview);
        app.on_key(Key::BackTab);
        assert_eq!(app.page, Page::Reports);

        // 数字键直达
        app.on_key(Key::Char('2'));
        assert_eq!(app.page, Page::Plan);
        app.on_key(Key::Char('3'));
        assert_eq!(app.page, Page::Bills);
    }

    #[test]
    fn settings_updown_moves_sections_then_rows() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_focus = SettingsFocus::Menu;

        assert_eq!(
            app.settings_section,
            SettingsSection::Categories,
            "默认停在改分类，数据库运维不该占首屏"
        );
        assert!(matches!(app.on_key(Key::Down), Action::None));
        assert_eq!(
            app.settings_section,
            SettingsSection::Connection,
            "菜单态 ↓ 换分区"
        );
        app.on_key(Key::Down);
        assert_eq!(
            app.settings_section,
            SettingsSection::Categories,
            "分区也要能回绕"
        );
        app.on_key(Key::Up);
        assert_eq!(app.settings_section, SettingsSection::Connection);

        // 菜单态不该动列表选中行
        app.settings_section = SettingsSection::Categories;
        let before = app.settings_index();
        app.on_key(Key::Down);
        assert_eq!(
            app.settings_section,
            SettingsSection::Connection,
            "菜单态 ↓ 换的是分区"
        );
        app.on_key(Key::Up);
        assert_eq!(app.settings_index(), before, "菜单态不动条目");

        // Enter 进列表，之后 ↑↓ 才动条目
        assert_eq!(app.settings_focus, SettingsFocus::Menu);
        app.on_key(Key::Enter);
        assert_eq!(app.settings_focus, SettingsFocus::Detail);
        app.on_key(Key::Down);
        assert_ne!(app.settings_index(), before, "列表态 ↓ 换条目");

        // Esc 回菜单
        app.on_key(Key::Esc);
        assert_eq!(app.settings_focus, SettingsFocus::Menu);
    }

    #[test]
    fn enter_on_connection_section_stays_in_menu() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Connection;
        app.settings_focus = SettingsFocus::Menu;

        // 连接分区没有列表，Enter 不该把焦点送进一个不存在的列表
        app.on_key(Key::Enter);
        assert_eq!(app.settings_focus, SettingsFocus::Menu);
    }

    #[test]
    fn switching_pages_resets_settings_focus() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.settings_focus = SettingsFocus::Detail;

        app.on_key(Key::Right);
        app.on_key(Key::Left);
        assert_eq!(app.settings_focus, SettingsFocus::Menu, "回来应落在菜单上");
    }

    #[test]
    fn sync_shortcuts_only_fire_on_overview() {
        let mut app = sample_app();

        app.page = Page::Overview;
        assert!(matches!(
            app.on_key(Key::Char('s')),
            Action::SyncRequest(SyncKind::Today)
        ));
        assert!(matches!(
            app.on_key(Key::Char('w')),
            Action::SyncRequest(SyncKind::Week)
        ));

        // 别的页面按 s/w 不该意外开始连银行
        app.page = Page::Bills;
        assert!(matches!(app.on_key(Key::Char('s')), Action::None));
        assert!(matches!(app.on_key(Key::Char('w')), Action::None));
        app.page = Page::Settings;
        assert!(matches!(app.on_key(Key::Char('s')), Action::None));
    }

    #[test]
    fn enter_requests_resume_when_reconcile_is_pending() {
        let mut app = sample_app();
        app.page = Page::Overview;

        // 没有待核对时 Enter 什么都不做
        assert!(matches!(app.on_key(Key::Enter), Action::None));

        app.pending_sync.push(PendingSync {
            transaction: crate::bank::model::BankTransaction {
                date_time: NaiveDate::from_ymd_opt(2026, 10, 5)
                    .unwrap()
                    .and_hms_opt(12, 0, 0)
                    .unwrap(),
                kind: BillKind::Expense,
                amount: 1234,
                memo: "午饭".to_string(),
                balance: None,
                raw_type: "网联协议支付".to_string(),
                fingerprint: "fp-pending".to_string(),
            },
            suggestion: crate::bank::service::Suggestion {
                book: "微信".to_string(),
                category: "餐饮".to_string(),
                source: crate::bank::service::SuggestionSource::Keyword("鸡排".to_string()),
            },
        });
        app.sync_total = 1;

        assert!(
            matches!(app.on_key(Key::Enter), Action::ResumeSync),
            "有没核完的流水时 Enter 应恢复核对"
        );
    }

    #[test]
    fn overview_home_end_and_paging_move_the_inspected_day() {
        let mut app = sample_app();
        app.page = Page::Overview;

        app.on_key(Key::Home);
        assert_eq!(app.inspected_day(), 1);
        app.on_key(Key::End);
        assert_eq!(app.inspected_day(), 31, "2026-10 有 31 天");

        app.on_key(Key::Home);
        app.on_key(Key::PageDown);
        assert_eq!(app.inspected_day(), 8, "PageDown 往后一周");
        app.on_key(Key::PageUp);
        assert_eq!(app.inspected_day(), 1, "到头就停住");
    }

    #[test]
    fn brackets_are_ignored_on_settings_page() {
        let mut app = sample_app();
        app.page = Page::Settings;

        assert!(matches!(app.on_key(Key::Char('[')), Action::None));
        assert!(matches!(app.on_key(Key::Char(']')), Action::None));
    }

    #[test]
    fn months_and_years_open_month_settings_on_e() {
        for tab in [ReportTab::Month, ReportTab::Year] {
            let mut app = sample_app();
            app.page = Page::Reports;
            app.report_tab = tab;
            app.on_key(Key::Char('e'));
            match &app.modal {
                Some(Modal::Form(form)) => assert_eq!(
                    form.kind,
                    FormKind::SetStartBalance,
                    "报表页按 e 应打开本月设置（和底栏提示一致）"
                ),
                other => panic!("{tab:?} 页按 e 应打开本月设置，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn enter_on_bills_opens_the_selected_bill() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.bills = vec![sample_bill(7, 1000)];
        app.selected[Page::Bills.index()] = 0;

        app.on_key(Key::Enter);
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::NewBill, "编辑账单复用新账单表单");
                assert_eq!(form.target, Some(EditTarget::Bill(7)));
                assert_eq!(form.value("金额"), "10.00");
            }
            other => panic!("账单页 Enter 应打开编辑表单，实际 {other:?}"),
        }
    }

    #[test]
    fn setup_state_only_allows_navigation() {
        let mut app = sample_app();
        app.mark_needs_setup();
        app.page = Page::Overview;

        // 记账 / 删除 / 同步这些动作在没连上库时都不该开表单或发动作
        assert!(matches!(app.on_key(Key::Char('a')), Action::None));
        assert!(matches!(app.on_key(Key::Char('d')), Action::None));
        assert!(matches!(app.on_key(Key::Char('s')), Action::None));
        assert!(app.modal.is_none(), "不该弹出任何表单");

        // 但仍能翻到设置页去配库
        assert!(matches!(app.on_key(Key::Char('6')), Action::Refresh));
        assert_eq!(app.page, Page::Settings);
    }

    #[test]
    fn settings_selection_is_per_section() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        app.settings_focus = SettingsFocus::Detail;

        assert_eq!(app.settings_row_count(), 2);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 1);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 0, "到底应回绕");

        // 连接分区没有列表，上下键无害，也不动分类的选中行
        app.settings_section = SettingsSection::Connection;
        assert_eq!(app.settings_row_count(), 0);
        app.on_key(Key::Down);
        assert_eq!(app.settings_index(), 0);

        app.settings_section = SettingsSection::Categories;
        assert_eq!(app.settings_index(), 0, "回到分类分区，选中行还是原来那个");
    }

    #[test]
    fn mark_needs_setup_clears_data_and_flags() {
        let mut app = sample_app();
        assert!(!app.needs_setup, "有连接时不该进引导状态");

        app.mark_needs_setup();

        assert!(app.needs_setup, "应标记为待配置");
        assert!(app.books.is_empty());
        assert!(app.categories.is_empty());
        assert!(app.bills.is_empty());
        assert!(app.report.is_none());
        assert!(app.progress.is_none());
        assert!(app.modal.is_none());
    }

    fn sample_bill(id: u64, amount: i64) -> BillEntity {
        BillEntity {
            id,
            kind: BillKind::Expense,
            amount,
            book: BillBook {
                id: 1,
                name: "微信".into(),
            },
            category: Category {
                id: 1,
                parent_id: None,
                name: "餐饮".into(),
            },
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
                assert_eq!(form.value("计划"), "微信");
                assert_eq!(form.value("分类"), "餐饮");
            }
            other => panic!("账单页应打开编辑表单，实际 {other:?}"),
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
            vec![
                "(顶层)".to_string(),
                "餐饮".to_string(),
                "  午餐".to_string()
            ]
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

        // 焦点默认在「金额」；移到紧随其后的「分类」选择框上再回车
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.fields[form.focus].label, "金额"),
            other => panic!("{other:?}"),
        }
        app.on_key(Key::Tab);
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.fields[form.focus].label, "分类"),
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
                assert_eq!(form.value("分类"), "午餐");
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
            Some(Modal::Form(form)) => assert_eq!(form.value("计划"), "支付宝"),
            other => panic!("应回到表单，实际 {other:?}"),
        }
    }

    #[test]
    fn brackets_switch_period_depending_on_report_tab() {
        let mut app = sample_app();
        let month = app.month;
        let year = app.year;

        app.page = Page::Reports;
        app.report_tab = ReportTab::Year;
        assert!(matches!(app.on_key(Key::Char(']')), Action::ShiftYear(1)));
        assert!(matches!(app.on_key(Key::Char('[')), Action::ShiftYear(-1)));
        assert_eq!(app.month, month, "年报换年不应动月份");
        assert_eq!(app.year, year, "动作由外壳执行，状态机本身不变更年份");

        app.report_tab = ReportTab::Month;
        assert!(matches!(app.on_key(Key::Char(']')), Action::ShiftMonth(1)));
    }

    #[test]
    fn equals_evaluates_amount_expression() {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.on_key(Key::Char('a'));

        // 焦点默认就在「金额」上
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
        app.settings_section = SettingsSection::Categories;
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
        app.settings_section = SettingsSection::Connection;

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
            Action::SaveConnection(ConnectionTarget::Remote(settings)) => {
                assert_eq!(settings.host, "127.0.0.1");
                assert_eq!(settings.port, 6543);
                assert_eq!(settings.database, "biller");
            }
            other => panic!("回车应产出 SaveConnection，实际 {other:?}"),
        }
        assert!(app.modal.is_none(), "保存后表单应关闭");
    }

    #[test]
    fn connection_form_can_switch_to_local_file() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Connection;
        app.connection_url = "sqlite:C:/Users/test/.biller/biller.db".to_string();

        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.value("模式"), "本地文件", "本地连接应预选本地模式");
                assert_eq!(form.value("文件路径"), TEST_SQLITE_PATH);
            }
            other => panic!("应打开连接表单，实际 {other:?}"),
        }

        // 直接回车提交：本地模式只要文件路径，不缺 PG 那几个必填项
        let action = app.on_key(Key::Enter);
        match action {
            Action::SaveConnection(ConnectionTarget::Local { path }) => {
                assert_eq!(path, TEST_SQLITE_PATH);
            }
            other => panic!("应产出本地目标，实际 {other:?}"),
        }
    }

    #[test]
    fn connection_form_rejects_bad_port() {
        let mut app = sample_app();
        app.page = Page::Settings;
        app.settings_section = SettingsSection::Connection;
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
        app.page = Page::Reports;

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
    fn x_toggles_plan_calendar_on_plan_page_only() {
        let mut app = sample_app();
        app.page = Page::Plan;
        app.plans = vec![PlanRow::new(plan_row("房租", Some(1_000_00)), 0)];
        assert!(matches!(
            app.on_key(Key::Char('x')),
            Action::TogglePlanCalendar(1)
        ));

        // 账单页不再有「单笔账单计不计入」这回事
        app.page = Page::Bills;
        assert!(matches!(app.on_key(Key::Char('x')), Action::None));

        app.page = Page::Settings;
        app.settings_section = SettingsSection::Categories;
        assert!(matches!(app.on_key(Key::Char('x')), Action::None));
    }

    #[test]
    fn overview_e_edits_start_balance_only() {
        let mut app = sample_app();

        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::SetStartBalance);
                assert_eq!(form.value("起始金额"), "1,000.00");
                // 计划已经搬到计划页：本月设置只剩起始金额一项
                assert_eq!(form.fields.len(), 1);
            }
            other => panic!("概览页应打开本月设置，实际 {other:?}"),
        }
    }

    #[test]
    fn plan_page_opens_plan_forms() {
        let mut app = sample_app();
        app.page = Page::Plan;
        app.plans = vec![
            PlanRow::new(plan_row("日常开销", Some(3_000_00)), 0),
            PlanRow::new(plan_row("存款", None), 0),
        ];

        app.on_key(Key::Char('a'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::PlanItem);
                assert_eq!(form.title, "新建计划");
                assert!(form.target.is_none());
            }
            other => panic!("计划页按 a 应打开新建计划，实际 {other:?}"),
        }

        app.modal = None;
        app.selected[Page::Plan.slot()] = 1;
        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.kind, FormKind::PlanItem);
                assert_eq!(form.title, "编辑计划");
                assert_eq!(form.value("名称"), "存款");
                // 没额度的计划，额度字段留空——留空就是不设额度
                assert_eq!(form.value("额度"), "");
                assert_eq!(form.value("类型"), "开销");
                // 编辑目标是账本 id（plan_row 造的这条 id 是 1），不是列表下标
                assert_eq!(form.target, Some(EditTarget::PlanItem(1)));
            }
            other => panic!("计划页按 e 应打开编辑计划，实际 {other:?}"),
        }
    }

    /// 计划页按 d 不是直接弹确认框：得先去服务层查名下账单数，带账单才提示会搬去默认计划。
    #[test]
    fn delete_on_plan_page_asks_for_a_bill_count_first() {
        let mut app = sample_app();
        app.page = Page::Plan;
        app.plans = vec![PlanRow::new(plan_row("房租", Some(1_000_00)), 0)];

        match app.on_key(Key::Char('d')) {
            Action::PrepareDeletePlan(id) => assert_eq!(id, 1, "按计划 id 定位，不是列表下标"),
            other => panic!("计划页 d 应先查账单数，实际 {other:?}"),
        }
        assert!(app.modal.is_none(), "查数没回来之前不该弹确认框");
    }

    /// 金额输入框 → 分：带运算符先求值，纯数字保留千分位/两位小数的宽容度。
    #[test]
    fn amount_field_evaluates_arithmetic_for_both_forms() {
        assert_eq!(amount_from_field("1000+500").unwrap(), 150_000);
        assert_eq!(amount_from_field("(2500+300)*2").unwrap(), 560_000);
        assert_eq!(amount_from_field("1,000.50").unwrap(), 100_050);
        assert!(amount_from_field("abc").is_err());
        assert!(amount_from_field("12+").is_err());
    }

    /// 本月设置的起始金额也是金额字段：算式照样按 = 求值。
    #[test]
    fn month_settings_accepts_arithmetic() {
        let mut app = sample_app();
        app.page = Page::Overview;
        app.on_key(Key::Char('e'));

        // 清掉预填的 1,000.00，再输 1000+500
        for _ in "1,000.00".chars() {
            app.on_key(Key::Backspace);
        }
        for character in "1000+500".chars() {
            app.on_key(Key::Char(character));
        }
        app.on_key(Key::Char('='));

        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.value("起始金额"), "1,500.00"),
            other => panic!("本月设置应打开起始金额表单，实际 {other:?}"),
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

    /// 造一笔指定日期的账单（分页测试要控制日期分组）。
    fn bill_on(date: NaiveDate, id: u64) -> BillEntity {
        let mut bill = sample_bill(id, 100);
        bill.created_at = date.and_hms_opt(12, 0, 0).unwrap().and_utc();
        bill
    }

    /// 往 app 里塞 days 天、每天 per_day 笔账单（日期从 10-25 往前倒）。
    fn app_with_bills(days: u32, per_day: u32, viewport: u16) -> App {
        let mut app = sample_app();
        app.page = Page::Bills;
        app.viewport_height = viewport;

        let mut id = 1;
        let mut bills = Vec::new();
        for offset in 0..days {
            let date = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap()
                - chrono::Duration::days(offset as i64);
            for _ in 0..per_day {
                bills.push(bill_on(date, id));
                id += 1;
            }
        }
        app.bills = bills;
        app
    }

    #[test]
    fn bill_page_budget_follows_the_viewport() {
        let mut app = sample_app();
        app.viewport_height = 40;
        // 40 行终端 → 内容区 34 行左右，一页远不止 10 条
        assert_eq!(app.bill_page_budget(), 34);
        assert!(app.bill_page_budget() > 10, "高终端应能一页放下更多账单");

        app.viewport_height = 10;
        assert_eq!(app.bill_page_budget(), 4);
        app.viewport_height = 1;
        assert_eq!(app.bill_page_budget(), 2, "极矮时也要留出行数，不能为 0");
    }

    #[test]
    fn tall_viewport_shows_every_bill_on_a_single_page() {
        let app = app_with_bills(3, 3, 40);

        let paging = app.bill_page();
        assert_eq!(paging.pages, 1, "9 笔 + 3 个日期表头在 40 行终端放得下");
        assert_eq!(paging.start, 0);
        assert_eq!(paging.end, 9, "一页就要覆盖全部账单，不能只显示 10 条然后空着");
    }

    #[test]
    fn short_viewport_splits_pages_on_day_boundaries() {
        // 视口 15 → 每页 9 行；每天 2 笔 = 3 行（表头 + 2 行），正好三整天
        let app = app_with_bills(6, 2, 15);
        assert_eq!(app.bill_page_budget(), 9);

        let first = app.bill_page();
        assert_eq!(first.page, 0);
        assert_eq!(first.pages, 2, "6 天 × 3 行 = 18 行，两页");
        assert_eq!(first.end, 6, "第一页装 3 天共 6 笔，日期不被劈开");

        let second = app.bill_page_of(12, 9, 9);
        assert_eq!(second.page, 1);
        assert_eq!(second.start, 6);
        assert_eq!(second.end, 12);
    }

    #[test]
    fn page_down_jumps_a_whole_page_and_page_up_comes_back() {
        let mut app = app_with_bills(6, 2, 15);
        let page = app.bill_page();

        app.on_key(Key::PageDown);
        assert_eq!(
            app.selected_index(Page::Bills),
            page.end,
            "PgDn 应落到下一页第一笔，而不是走 10 条"
        );
        assert_eq!(app.bill_page().page, 1);

        app.on_key(Key::PageUp);
        assert_eq!(app.bill_page().page, 0, "PgUp 应回到上一页");
    }

    #[test]
    fn plan_form_asks_name_then_kind_then_limit() {
        let form = Form::plan_item(&[], None);

        assert_eq!(form.fields[0].label, "名称");
        assert_eq!(form.fields[1].label, "类型");
        assert!(form.fields[1].is_choice(), "类型是选项：开销 / 收入");
        assert_eq!(form.value("类型"), "开销", "新建默认开销计划");
        assert_eq!(form.fields[2].label, "额度");
        assert!(!form.fields[2].is_choice(), "额度是手填，不是选项");
        assert_eq!(form.value("额度"), "", "新建时额度留空");
        assert_eq!(form.fields[3].label, "计入日历");
        assert!(form.fields[3].is_choice(), "计入日历是选项：计入 / 不计入");
        assert_eq!(form.value("计入日历"), "计入", "新建默认计入日历");
        assert_eq!(plan_limit_from_form(&form).unwrap(), None, "留空就是不设额度");
        assert_eq!(plan_kind_from_form(&form), PlanKind::Expense);
        assert!(plan_calendar_from_form(&form));
    }

    #[test]
    fn plan_form_prefills_the_existing_limit() {
        let rows = vec![
            PlanRow::new(plan_row("吃喝", Some(30_00)), 0),
            PlanRow::new(plan_row("零花", None), 0),
        ];

        let form = Form::plan_item(&rows, Some(0));
        assert_eq!(form.value("名称"), "吃喝");
        assert_eq!(form.value("类型"), "开销");
        assert_eq!(form.value("额度"), "30.00");
        assert_eq!(form.value("计入日历"), "计入");
        assert_eq!(form.focus, 2, "编辑时焦点落在额度上");

        let mut empty = Form::plan_item(&rows, Some(1));
        empty.fields[2].value.clear();
        assert_eq!(plan_limit_from_form(&empty).unwrap(), None);
    }

    #[test]
    fn plan_form_prefills_an_income_plan_as_income() {
        let income = PlanRow::new(
            Plan::new(
                BillBook {
                    id: 9,
                    name: "工资".into(),
                },
                Some(8_000_00),
                PlanKind::Income,
            ),
            6_000_00,
        );

        let form = Form::plan_item(&[income], Some(0));
        assert_eq!(form.value("类型"), "收入", "收入计划打开就是收入，不要回落到默认");
        assert_eq!(form.value("额度"), "8,000.00");
        assert_eq!(plan_kind_from_form(&form), PlanKind::Income);
        assert!(plan_calendar_from_form(&form));
    }

    /// 不计入日历的计划，编辑表单要把「不计入」预填出来。
    #[test]
    fn plan_form_prefills_an_out_of_calendar_plan() {
        let row = PlanRow::new(
            Plan::new(
                BillBook {
                    id: 3,
                    name: "报销".into(),
                },
                Some(1_000_00),
                PlanKind::Expense,
            )
            .with_calendar(false),
            0,
        );

        let form = Form::plan_item(&[row], Some(0));
        assert_eq!(form.value("计入日历"), "不计入");
        assert!(!plan_calendar_from_form(&form));
    }

    #[test]
    fn plan_form_rejects_an_empty_name_and_a_zero_limit() {
        let mut form = Form::plan_item(&[], None);
        assert!(plan_name(&form).is_err(), "计划名不能为空");

        form.fields[0].value = "房租".to_string();
        form.fields[2].value = "0".to_string();
        assert!(
            plan_limit_from_form(&form).is_err(),
            "额度 0 要报错，想不设额度就留空"
        );

        form.fields[2].value = "3000 - 500".to_string();
        assert_eq!(
            plan_limit_from_form(&form).unwrap(),
            Some(2_500_00),
            "额度也支持算式"
        );
    }
}
