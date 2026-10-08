//! 账单领域的数据结构：方向 [`BillKind`] 与账单 [`BillEntity`]。

use chrono::{DateTime, Utc};

use crate::bill_book::model::BillBook;
use crate::category::model::Category;
use crate::common::types::{AmountType, BillId};

/// 账单方向：收入 / 支出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillKind {
    Income,
    Expense,
}

impl BillKind {
    /// 落库用的字面量，与 `bills.kind` 的 CHECK 约束保持一致。
    pub fn as_str(&self) -> &'static str {
        match self {
            BillKind::Income => "income",
            BillKind::Expense => "expense",
        }
    }

    /// 从数据库/命令行文本解析，同时接受中英文写法。
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "income" | "in" | "收入" | "收" => Some(BillKind::Income),
            "expense" | "exp" | "out" | "支出" | "支" => Some(BillKind::Expense),
            _ => None,
        }
    }
}

impl std::fmt::Display for BillKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BillKind::Income => "收入",
            BillKind::Expense => "支出",
        })
    }
}

/// 一笔账单。
///
/// `amount` 恒为非负的「分」，方向由 `kind` 决定；
/// `book` / `category` 展开成完整对象，便于终端直接展示。
#[derive(Debug, Clone)]
pub struct BillEntity {
    pub id: BillId,
    pub kind: BillKind,
    pub amount: AmountType,
    pub book: BillBook,
    pub category: Category,
    pub created_at: DateTime<Utc>,
    pub remark: Option<String>,
    /// 不纳入统计：报销、代付、走账这类过手钱，不该算进收支与开销计划。
    pub excluded: bool,
}

impl BillEntity {
    /// 带符号金额，便于直接累加
    pub fn signed_amount(&self) -> AmountType {
        match self.kind {
            BillKind::Income => self.amount,
            BillKind::Expense => -self.amount,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> BillEntity {
        BillEntity {
            id: 1,
            kind: BillKind::Expense,
            amount: 1234,
            book: BillBook {
                id: 1,
                name: "微信".to_string(),
            },
            category: Category {
                id: 1,
                parent_id: None,
                name: "餐饮".to_string(),
            },
            created_at: Utc.with_ymd_and_hms(2026, 10, 5, 12, 30, 0).unwrap(),
            remark: Some("午饭".to_string()),
            excluded: false,
        }
    }

    #[test]
    fn round_trips_db_literal() {
        for kind in [BillKind::Income, BillKind::Expense] {
            assert_eq!(BillKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn accepts_chinese() {
        assert_eq!(BillKind::parse("收入"), Some(BillKind::Income));
        assert_eq!(BillKind::parse("支出"), Some(BillKind::Expense));
        assert_eq!(BillKind::parse("EXPENSE"), Some(BillKind::Expense));
        assert_eq!(BillKind::parse("unknown"), None);
    }

    #[test]
    fn signs_amount_by_kind() {
        let mut bill = sample();
        assert_eq!(bill.signed_amount(), -1234);
        bill.kind = BillKind::Income;
        assert_eq!(bill.signed_amount(), 1234);
    }
}
