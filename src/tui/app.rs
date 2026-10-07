//! 交互式 TUI 的状态机。
//!
//! 本模块只有纯逻辑：
//! 按键 → [`Action`]，以及 [`Action`] → 调用各领域 `service`。
//! 渲染与终端 IO 分别在 `view.rs` / `event.rs`，因此这里可以脱离终端测试。

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, Months, NaiveDate, Utc};

use crate::bill::model::{BillEntity, BillKind};
use crate::bill::service::{BillQuery, NewBill};
use crate::bill_book::model::BillBook;
use crate::category::model::Category;
use crate::category::service::{CategoryNode, descendants_of, subtree_of};
use crate::common::db::ConnectionSettings;
use crate::common::money;
use crate::common::types::{AmountType, CategoryId};
use crate::month::service::{MonthReport, YearReport};
use crate::tui::Services;
use crate::tui::input::Key;

/// 页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Bills,
    Books,
    Categories,
    Months,
    Years,
    Settings,
}

impl Page {
    pub const ALL: [Page; 7] = [
        Page::Overview,
        Page::Bills,
        Page::Books,
        Page::Categories,
        Page::Months,
        Page::Years,
        Page::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Overview => "概览",
            Page::Bills => "账单",
            Page::Books => "账本",
            Page::Categories => "分类",
            Page::Months => "月报",
            Page::Years => "年报",
            Page::Settings => "设置",
        }
    }

    /// 该页是否支持上下选择行。
    pub fn is_list(self) -> bool {
        matches!(
            self,
            Page::Bills | Page::Books | Page::Categories | Page::Months
        )
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

    /// 设置某月起始金额（概览页按 `e`）。
    pub fn set_start_balance(month: NaiveDate, current: AmountType) -> Self {
        Self {
            kind: FormKind::SetStartBalance,
            title: "设置月起始金额",
            fields: vec![Field::text(
                "起始金额",
                money::format_amount(current),
                "元，例如 1000 或 1000.50",
            )],
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
    pub fn migrate_target(settings: &ConnectionSettings) -> Self {
        let mut form = Self::connection(settings);
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
        Self {
            page: Page::Overview,
            month,
            year: month.year(),
            connection: ConnectionSettings::from_url(connection_url),
            connection_url: connection_url.to_string(),
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

    fn today() -> NaiveDate {
        Utc::now().date_naive()
    }

    /// 当前页可供选择的行数。
    pub fn row_count(&self, page: Page) -> usize {
        match page {
            Page::Bills => self.bills.len(),
            Page::Books => self.books.len(),
            Page::Categories => self.categories.len(),
            Page::Months => self.month_expense_categories().len(),
            Page::Overview | Page::Years | Page::Settings => 0,
        }
    }

    /// 月报页的分类列表：按支出金额从多到少（金额相同按 id 稳定排序）。
    pub fn month_expense_categories(&self) -> Vec<(CategoryId, AmountType)> {
        let Some(report) = &self.report else {
            return Vec::new();
        };

        let mut entries: Vec<(CategoryId, AmountType)> = report
            .collection
            .expense_by_category
            .iter()
            .map(|(id, amount)| (*id, *amount))
            .collect();
        entries.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        entries
    }

    /// 月报页右栏：当前选中的那个分类在本月的账单。
    pub fn month_category_bills(&self) -> Vec<&BillEntity> {
        let entries = self.month_expense_categories();
        let Some((category_id, _)) = entries.get(self.selected_index(Page::Months)) else {
            return Vec::new();
        };

        self.bills
            .iter()
            .filter(|bill| bill.category.id == *category_id)
            .collect()
    }

    /// 月报页当前选中的分类名。
    pub fn month_selected_category_name(&self) -> Option<String> {
        let entries = self.month_expense_categories();
        let (category_id, _) = entries.get(self.selected_index(Page::Months))?;
        self.report
            .as_ref()?
            .category_names
            .get(category_id)
            .cloned()
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
            Page::Books => Form::new_book(),
            Page::Categories => Form::new_category(self.parent_options(None)),
            Page::Settings => Form::connection(&self.connection),
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
            // 概览页的「可编辑内容」就是本月起始金额；设置页就是连接参数
            Page::Overview => {
                let current = self
                    .report
                    .as_ref()
                    .map(|report| report.collection.start_balance)
                    .unwrap_or(0);
                Some(Form::set_start_balance(self.month, current))
            }
            Page::Settings => Some(Form::connection(&self.connection)),
            Page::Bills => self
                .bills
                .get(self.selected_index(Page::Bills))
                .map(|bill| Form::edit_bill(bill, self.book_names(), self.category_names())),
            Page::Books => self
                .books
                .get(self.selected_index(Page::Books))
                .map(Form::edit_book),
            Page::Categories => self
                .categories
                .get(self.selected_index(Page::Categories))
                .cloned()
                .map(|node| {
                    let options = self.parent_options(Some(node.category.id));
                    let label = match node.category.parent_id {
                        Some(parent_id) => self.parent_label(parent_id),
                        None => "(顶层)".to_string(),
                    };
                    Form::edit_category(&node.category, options, &label)
                }),
            Page::Months | Page::Years => None,
        };

        match form {
            Some(form) => self.modal = Some(Modal::Form(form)),
            None => self.set_info("当前页没有可编辑的内容"),
        }
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
            Page::Books => self
                .books
                .get(self.selected_index(Page::Books))
                .map(|book| {
                    (
                        format!("确定要删除账本「{}」吗？此操作不可撤销。", book.name),
                        Action::DeleteBook(book.id),
                    )
                }),
            Page::Categories => self
                .categories
                .get(self.selected_index(Page::Categories))
                .cloned()
                .map(|node| {
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

                    (
                        message,
                        Action::DeleteCategory {
                            id: node.category.id,
                            recursive: !children.is_empty(),
                        },
                    )
                }),
            Page::Overview | Page::Months | Page::Years | Page::Settings => None,
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
                self.move_selection(-1);
                Action::None
            }
            Key::Down => {
                self.move_selection(1);
                Action::None
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
                self.selected[self.page.index()] = 0;
                Action::None
            }
            Key::End => {
                let count = self.row_count(self.page);
                self.selected[self.page.index()] = count.saturating_sub(1);
                Action::None
            }
            // ←/→ 切换页面（与 Tab 一致）；换月改用 [ ]
            Key::Left => {
                self.page = self.page.previous();
                Action::Refresh
            }
            Key::Right => {
                self.page = self.page.next();
                Action::Refresh
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
            Key::Char('d') | Key::Delete => {
                self.request_delete();
                Action::None
            }
            Key::Char('m') => {
                let today = Self::today();
                self.month = today;
                self.year = today.year();
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
                services.bills.remove(id).await?;
                self.set_info(format!("已删除账单 #{id}"));
                self.reload(services).await?;
            }
            Action::DeleteBook(id) => {
                let book = services.books.remove(&id.to_string()).await?;
                self.set_info(format!("已删除账本「{}」", book.name));
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
                        "已删除分类「{}」及其 {} 个子分类",
                        root.name,
                        removed.len() - 1
                    ));
                } else {
                    self.set_info(format!("已删除分类「{}」", root.name));
                }
                self.reload(services).await?;
            }
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
            services.months.set_start_balance(month, amount).await?;
            Ok(format!(
                "{} 起始金额已设为 {} 元",
                crate::common::time::format_month(month),
                money::format_amount(amount)
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

        let mut category_names = HashMap::new();
        category_names.insert(1, "餐饮".to_string());
        category_names.insert(3, "工资".to_string());

        app.report = Some(MonthReport {
            collection,
            book_names: HashMap::new(),
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
        assert_eq!(app.page, Page::Categories);
        app.on_key(Key::Char('7'));
        assert_eq!(app.page, Page::Settings, "第 7 个数字键应跳到设置页");
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
    fn arrow_keys_switch_pages_without_touching_month() {
        let mut app = sample_app();
        let month = app.month;

        app.on_key(Key::Right);
        assert_eq!(app.page, Page::Bills);
        app.on_key(Key::Right);
        assert_eq!(app.page, Page::Books);
        app.on_key(Key::Left);
        assert_eq!(app.page, Page::Bills);
        app.on_key(Key::Left);
        assert_eq!(app.page, Page::Overview);
        app.on_key(Key::Left);
        assert_eq!(
            app.page,
            *Page::ALL.last().expect("至少有一页"),
            "从第一页往前应回绕到最后一页"
        );

        assert_eq!(app.month, month, "左右箭头不应改变月份");
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
        app.page = Page::Categories;
        app.selected[Page::Categories.index()] = 0; // 餐饮（有子分类 午餐）
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
        app.page = Page::Categories;
        app.selected[Page::Categories.index()] = 1; // 午餐（叶子）
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
        app.page = Page::Books;
        app.books = vec![
            BillBook { id: 1, name: "微信".into() },
            BillBook { id: 2, name: "支付宝".into() },
        ];
        app.on_key(Key::Up);
        assert_eq!(app.selected_index(Page::Books), 1, "向上应回绕到末行");
        app.on_key(Key::Down);
        assert_eq!(app.selected_index(Page::Books), 0, "向下应回绕到首行");

        app.books.truncate(1);
        app.clamp_selection();
        assert_eq!(app.selected_index(Page::Books), 0);
    }

    #[test]
    fn a_key_opens_page_specific_form() {
        let mut app = sample_app();
        app.page = Page::Books;
        app.on_key(Key::Char('a'));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.kind, FormKind::NewBook),
            other => panic!("账本页应打开新增账本，实际 {other:?}"),
        }

        let mut app = sample_app();
        app.page = Page::Categories;
        app.on_key(Key::Char('a'));
        match &app.modal {
            Some(Modal::Form(form)) => assert_eq!(form.kind, FormKind::NewCategory),
            other => panic!("分类页应打开新增分类，实际 {other:?}"),
        }
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
        app.page = Page::Books;
        app.on_key(Key::Char('e'));
        match &app.modal {
            Some(Modal::Form(form)) => {
                assert_eq!(form.target, Some(EditTarget::Book(1)));
                assert_eq!(form.value("名称"), "微信");
            }
            other => panic!("账本页应打开编辑表单，实际 {other:?}"),
        }

        let mut app = sample_app();
        app.page = Page::Categories;
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
                assert_eq!(form.value("起始金额"), "1000.00");
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
        app.page = Page::Books;
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
}
