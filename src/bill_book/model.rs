use crate::common::money::format_amount;
use crate::common::types::{AmountType, BookId};

/// 账本。
///
/// 账本已经并入「计划」：一个账本就是一个资金池，账单记在哪个账本，就等于归到哪个计划。
/// 上限存在 `bill_books.plan_limit`，由 [`Plan`] 带出来，所以账单表里那一列 `book_id`
/// 既不用改名也不用迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillBook {
    pub id: BookId,
    pub name: String,
}

/// 计划的类型：钱从哪里来、到哪里去。
///
/// * **收入计划**（如「工资」）：盯的是「这个月进账多少」，和「目标」比；
/// * **开销计划**（如「房租」）：盯的是「这个月花掉多少」，和「上限」比。
///
/// 两种计划共用一张 `bill_books` 表，靠 `plan_kind` 分组显示：收入在前、开销在后。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanKind {
    Income,
    #[default]
    Expense,
}

impl PlanKind {
    /// 给人看的中文名，TUI 表单和提示里用。
    pub fn label(self) -> &'static str {
        match self {
            Self::Income => "收入",
            Self::Expense => "开销",
        }
    }

    /// 存库用的稳定编码。
    pub fn code(self) -> &'static str {
        match self {
            Self::Income => "income",
            Self::Expense => "expense",
        }
    }

    /// 从库里的编码还原；不认识的旧值一律当开销计划。
    pub fn from_code(code: &str) -> Self {
        match code {
            "income" => Self::Income,
            _ => Self::Expense,
        }
    }
}

/// 计划 = 一个账本 + 可选额度 + 类型。
///
/// * **收入计划**：`limit = Some(每月目标)` 时看「已入 / 差距」；不设目标就只累计入账；
/// * **开销计划**：`limit = Some(每月上限)` 时超了要在概览和日历上提醒；
///   没上限就只从 0 开始累计「这个月花了多少」，不判超支。
///
/// 为什么是「账本 + 额度」而不是另建一张计划表：账单已经用 `book_id` 指向账本，
/// 再建一张表就得让账单同时挂两处，账早晚对不上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub book: BillBook,
    pub limit: Option<AmountType>,
    pub kind: PlanKind,
}

impl Plan {
    pub fn new(book: BillBook, limit: Option<AmountType>, kind: PlanKind) -> Self {
        Self { book, limit, kind }
    }

    pub fn id(&self) -> BookId {
        self.book.id
    }

    pub fn name(&self) -> &str {
        &self.book.name
    }

    /// 有没有额度。收入计划是「目标」，开销计划是「上限」。
    pub fn has_limit(&self) -> bool {
        self.limit.is_some_and(|limit| limit > 0)
    }

    /// 额度金额；没设额度按 0 算，方便只做累加的场合。
    pub fn limit_or_zero(&self) -> AmountType {
        self.limit.unwrap_or(0)
    }

    /// 把额度翻译成一句人话，删除确认、表单回执、CLI 都用它。
    pub fn describe_limit(&self) -> String {
        match (self.kind, self.limit) {
            (PlanKind::Expense, Some(limit)) if limit > 0 => {
                format!("每月 {} 元", format_amount(limit))
            }
            (PlanKind::Expense, _) => "不设上限，只从 0 开始累计".to_string(),
            (PlanKind::Income, Some(limit)) if limit > 0 => {
                format!("每月目标 {} 元", format_amount(limit))
            }
            (PlanKind::Income, _) => "不设目标，只累计入账".to_string(),
        }
    }
}
