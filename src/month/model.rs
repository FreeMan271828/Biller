use std::collections::HashMap;

use chrono::{Datelike, NaiveDate};

use crate::bill::model::{BillEntity, BillKind};
use crate::common::types::{AmountType, BookId, CategoryId};

#[derive(Debug, Clone)]
pub struct BillMonthCollection {
    /// 归属月份，统一取当月 1 号，例如 2026-10-01
    pub month: NaiveDate,

    pub start_balance: AmountType,

    pub total_income: AmountType,
    pub total_expense: AmountType,

    pub income_by_book: HashMap<BookId, AmountType>,
    pub expense_by_book: HashMap<BookId, AmountType>,

    pub income_by_category: HashMap<CategoryId, AmountType>,
    pub expense_by_category: HashMap<CategoryId, AmountType>,
}

impl BillMonthCollection {
    pub fn new(month: NaiveDate, start_balance: AmountType) -> Self {
        Self {
            month,
            start_balance,
            total_income: 0,
            total_expense: 0,
            income_by_book: HashMap::new(),
            expense_by_book: HashMap::new(),
            income_by_category: HashMap::new(),
            expense_by_category: HashMap::new(),
        }
    }

    /// 月末结余 = 起始金额 + 收入 - 支出
    pub fn remaining(&self) -> AmountType {
        self.start_balance + self.total_income - self.total_expense
    }

    /// 本月净额 = 收入 - 支出
    pub fn net(&self) -> AmountType {
        self.total_income - self.total_expense
    }

    pub fn from_bills(
        month: NaiveDate,
        start_balance: AmountType,
        bills: &[BillEntity],
    ) -> Self {
        let mut acc = Self::new(month, start_balance);

        for b in bills {
            if !Self::same_month(b.created_at.date_naive(), month) {
                continue;
            }
            let (total, by_book, by_cat) = match b.kind {
                BillKind::Income => (
                    &mut acc.total_income,
                    &mut acc.income_by_book,
                    &mut acc.income_by_category,
                ),
                BillKind::Expense => (
                    &mut acc.total_expense,
                    &mut acc.expense_by_book,
                    &mut acc.expense_by_category,
                ),
            };
            *total += b.amount;
            // 按账本/分类维度汇总，键分别取 book.id 与 category.id
            *by_book.entry(b.book.id).or_default() += b.amount;
            *by_cat.entry(b.category.id).or_default() += b.amount;
        }

        acc
    }

    fn same_month(a: NaiveDate, b: NaiveDate) -> bool {
        a.year() == b.year() && a.month() == b.month()
    }
}

/// 年度汇总：总额 + **按账本**分布 + 逐月走势。
///
/// 月份维度看分类（[`BillMonthCollection`]），年份维度看账本——两个口径互补。
#[derive(Debug, Clone)]
pub struct YearCollection {
    pub year: i32,

    pub total_income: AmountType,
    pub total_expense: AmountType,

    pub income_by_book: HashMap<BookId, AmountType>,
    pub expense_by_book: HashMap<BookId, AmountType>,

    /// 下标 0 表示 1 月，依次到 11 表示 12 月。
    pub income_by_month: [AmountType; 12],
    pub expense_by_month: [AmountType; 12],
}

impl YearCollection {
    pub fn new(year: i32) -> Self {
        Self {
            year,
            total_income: 0,
            total_expense: 0,
            income_by_book: HashMap::new(),
            expense_by_book: HashMap::new(),
            income_by_month: [0; 12],
            expense_by_month: [0; 12],
        }
    }

    /// 全年净额 = 收入 - 支出
    pub fn net(&self) -> AmountType {
        self.total_income - self.total_expense
    }

    pub fn from_bills(year: i32, bills: &[BillEntity]) -> Self {
        let mut acc = Self::new(year);

        for bill in bills {
            let date = bill.created_at.date_naive();
            if date.year() != year {
                continue;
            }
            let index = (date.month() - 1) as usize;

            match bill.kind {
                BillKind::Income => {
                    acc.total_income += bill.amount;
                    *acc.income_by_book.entry(bill.book.id).or_default() += bill.amount;
                    acc.income_by_month[index] += bill.amount;
                }
                BillKind::Expense => {
                    acc.total_expense += bill.amount;
                    *acc.expense_by_book.entry(bill.book.id).or_default() += bill.amount;
                    acc.expense_by_month[index] += bill.amount;
                }
            }
        }

        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bill_book::model::BillBook;
    use crate::category::model::Category;
    use chrono::{TimeZone, Utc};

    fn bill(
        id: u64,
        kind: BillKind,
        amount: i64,
        book: u64,
        category: u64,
        month: u32,
        day: u32,
    ) -> BillEntity {
        BillEntity {
            id,
            kind,
            amount,
            book: BillBook {
                id: book,
                name: format!("账本{book}"),
            },
            category: Category {
                id: category,
                parent_id: None,
                name: format!("分类{category}"),
            },
            created_at: Utc.with_ymd_and_hms(2026, month, day, 12, 0, 0).unwrap(),
            remark: None,
        }
    }

    /// 回归测试：汇总必须按 book.id / category.id 分桶，而不是账单 id。
    #[test]
    fn aggregates_by_book_and_category_not_bill_id() {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let bills = vec![
            bill(1, BillKind::Expense, 1000, 7, 3, 10, 2),
            bill(2, BillKind::Expense, 500, 7, 4, 10, 3),
            bill(3, BillKind::Income, 2000, 8, 5, 10, 4),
            // 别的月份，应被忽略
            bill(4, BillKind::Expense, 999, 7, 3, 9, 30),
        ];

        let collection = BillMonthCollection::from_bills(month, 10_000, &bills);

        assert_eq!(collection.total_expense, 1500);
        assert_eq!(collection.total_income, 2000);
        assert_eq!(collection.net(), 500);
        assert_eq!(collection.remaining(), 10_500);

        assert_eq!(collection.expense_by_book.get(&7), Some(&1500));
        assert_eq!(collection.expense_by_book.get(&1), None);
        assert_eq!(collection.income_by_book.get(&8), Some(&2000));

        assert_eq!(collection.expense_by_category.get(&3), Some(&1000));
        assert_eq!(collection.expense_by_category.get(&4), Some(&500));
        assert_eq!(collection.income_by_category.get(&5), Some(&2000));
    }

    /// 年度汇总按账本 + 逐月分桶，且只统计当年。
    #[test]
    fn year_collection_groups_by_book_and_month() {
        let bills = vec![
            bill(1, BillKind::Expense, 1000, 7, 3, 1, 15),
            bill(2, BillKind::Expense, 500, 7, 4, 3, 20),
            bill(3, BillKind::Income, 2000, 8, 5, 12, 1),
            // 上一年，应被忽略
            bill(4, BillKind::Expense, 999, 7, 3, 12, 31),
        ];
        // 上面最后一条实际是同年 12 月，换个年份构造真正的「跨年」数据
        let mut previous_year = bills.clone();
        previous_year[3].created_at = Utc.with_ymd_and_hms(2025, 12, 31, 0, 0, 0).unwrap();

        let collection = YearCollection::from_bills(2026, &previous_year);

        assert_eq!(collection.total_expense, 1500);
        assert_eq!(collection.total_income, 2000);
        assert_eq!(collection.net(), 500);

        assert_eq!(collection.expense_by_book.get(&7), Some(&1500));
        assert_eq!(collection.expense_by_book.get(&8), None);
        assert_eq!(collection.income_by_book.get(&8), Some(&2000));

        assert_eq!(collection.expense_by_month[0], 1000, "1 月");
        assert_eq!(collection.expense_by_month[2], 500, "3 月");
        assert_eq!(collection.income_by_month[11], 2000, "12 月");
    }
}
