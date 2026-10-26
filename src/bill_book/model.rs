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

/// 计划 = 一个账本 + 可选上限。
///
/// * **有上限**：`limit = Some(每月额度)`，超了要在概览和日历上提醒；
/// * **没上限**：`limit = None`，只从 0 开始累计「这个月花了多少」，不判超支。
///
/// 为什么是「账本 + 上限」而不是另建一张计划表：账单已经用 `book_id` 指向账本，
/// 再建一张表就得让账单同时挂两处，账早晚对不上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub book: BillBook,
    pub limit: Option<AmountType>,
}

impl Plan {
    pub fn new(book: BillBook, limit: Option<AmountType>) -> Self {
        Self { book, limit }
    }

    pub fn id(&self) -> BookId {
        self.book.id
    }

    pub fn name(&self) -> &str {
        &self.book.name
    }

    /// 有没有上限。没上限的计划只累计，不判超支。
    pub fn has_limit(&self) -> bool {
        self.limit.is_some_and(|limit| limit > 0)
    }

    /// 上限金额；没设上限按 0 算，方便只做累加的场合。
    pub fn limit_or_zero(&self) -> AmountType {
        self.limit.unwrap_or(0)
    }
}
