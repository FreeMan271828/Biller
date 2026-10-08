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

    /// 支出按「分类 + 账本」成对累计。
    ///
    /// 只看分类会把「钱从哪本账走」抹平：日常开销分散在多本账上时，
    /// 单看分类看不出某本账才是大头，只有成对比较才真实。
    pub expense_by_category_book: HashMap<(CategoryId, BookId), AmountType>,
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
            expense_by_category_book: HashMap::new(),
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

    /// 支出按「分类 + 账本」拆分的排行：金额从多到少。
    pub fn expense_pairs(&self) -> Vec<(CategoryId, BookId, AmountType)> {
        rank_pairs(self.expense_by_category_book.clone())
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
            // 注意：月报/年报**始终全额计入**。
            // bills.excluded 只影响「支出日历」与「开销计划」，不影响报表口径。

            match b.kind {
                BillKind::Income => {
                    acc.total_income += b.amount;
                    *acc.income_by_book.entry(b.book.id).or_default() += b.amount;
                    *acc.income_by_category.entry(b.category.id).or_default() += b.amount;
                }
                BillKind::Expense => {
                    acc.total_expense += b.amount;
                    // 按账本 / 分类维度汇总，键分别取 book.id 与 category.id
                    *acc.expense_by_book.entry(b.book.id).or_default() += b.amount;
                    *acc.expense_by_category.entry(b.category.id).or_default() += b.amount;
                    // 再按「分类 + 账本」成对累计
                    *acc.expense_by_category_book
                        .entry((b.category.id, b.book.id))
                        .or_default() += b.amount;
                }
            }
        }

        acc
    }

    fn same_month(a: NaiveDate, b: NaiveDate) -> bool {
        a.year() == b.year() && a.month() == b.month()
    }
}

/// 把「分类 + 账本」的累计表排成金额降序的排行。
///
/// 月报（整月）与概览（今天）共用，保证两处排序口径一致：金额相同时按分类、
/// 账本 id 排序，终端里的顺序才稳定可比。
pub fn rank_pairs(
    totals: HashMap<(CategoryId, BookId), AmountType>,
) -> Vec<(CategoryId, BookId, AmountType)> {
    let mut rows: Vec<(CategoryId, BookId, AmountType)> = totals
        .into_iter()
        .map(|((category, book), amount)| (category, book, amount))
        .collect();

    rows.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then(left.0.cmp(&right.0))
            .then(left.1.cmp(&right.1))
    });

    rows
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
            // 年报同样全额计入，不看 bills.excluded
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
            excluded: false,
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

    /// 回归测试：支出必须能按「分类 + 账本」拆开比较。
    #[test]
    fn splits_expense_by_category_and_book() {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let bills = vec![
            // 同一个分类，钱却是从两本不同的账走的
            bill(1, BillKind::Expense, 5000, 9, 3, 10, 1),
            bill(2, BillKind::Expense, 1000, 7, 3, 10, 2),
            bill(3, BillKind::Expense, 200, 7, 4, 10, 3),
            // 收入不进支出排行
            bill(4, BillKind::Income, 8000, 7, 5, 10, 4),
        ];

        let collection = BillMonthCollection::from_bills(month, 0, &bills);

        assert_eq!(
            collection.expense_pairs(),
            vec![(3, 9, 5000), (3, 7, 1000), (4, 7, 200)],
            "同一分类要按账本拆开，并按金额降序"
        );

        // 分类口径仍然并存：它是两本账的合计
        assert_eq!(collection.expense_by_category.get(&3), Some(&6000));
        assert_eq!(collection.expense_by_book.get(&7), Some(&1200));
        assert_eq!(collection.expense_by_book.get(&9), Some(&5000));
    }

    /// 回归测试：`excluded` 只影响日历与计划，报表口径必须全额计入。
    #[test]
    fn reports_always_include_excluded_bills() {
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut excluded = bill(1, BillKind::Expense, 1234, 7, 3, 10, 2);
        excluded.excluded = true;

        let bills = vec![excluded, bill(2, BillKind::Income, 500, 7, 3, 10, 3)];

        let collection = BillMonthCollection::from_bills(month, 0, &bills);
        assert_eq!(collection.total_expense, 1234, "月报不看 excluded");
        assert_eq!(collection.expense_by_category.get(&3), Some(&1234));

        let year = YearCollection::from_bills(2026, &bills);
        assert_eq!(year.total_expense, 1234, "年报同样不看 excluded");
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
