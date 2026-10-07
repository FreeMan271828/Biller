//! 月度记账业务规则：把「起始金额 + 当月账单」汇总成 [`BillMonthCollection`]。
//!
//! 跨领域只依赖对方的 service：账单走 [`BillService`]，
//! 账本/分类的 id→名称映射走 [`BookService`] / [`CategoryService`]。

use std::collections::HashMap;

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::PgPool;

use crate::bill::service::BillService;
use crate::bill_book::service::BookService;
use crate::category::service::CategoryService;
use crate::common::types::{AmountType, BookId, CategoryId};
use crate::month::dao::MonthDao;
use crate::month::model::{BillMonthCollection, YearCollection};

/// 月度报表：汇总结果 + id→名称映射，便于终端直接展示。
#[derive(Debug, Clone)]
pub struct MonthReport {
    pub collection: BillMonthCollection,
    pub book_names: HashMap<BookId, String>,
    pub category_names: HashMap<CategoryId, String>,
}

/// 年度报表：按账本口径 + 逐月走势。
#[derive(Debug, Clone)]
pub struct YearReport {
    pub collection: YearCollection,
    pub book_names: HashMap<BookId, String>,
}

#[derive(Clone)]
pub struct MonthService {
    dao: MonthDao,
    bills: BillService,
    books: BookService,
    categories: CategoryService,
}

impl MonthService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            dao: MonthDao::new(pool.clone()),
            bills: BillService::new(pool.clone()),
            books: BookService::new(pool.clone()),
            categories: CategoryService::new(pool),
        }
    }

    /// 设置某月起始金额。
    pub async fn set_start_balance(&self, month: NaiveDate, amount: AmountType) -> Result<()> {
        self.dao.set_start_balance(month, amount).await
    }

    /// 生成某月报表。
    pub async fn report(&self, month: NaiveDate) -> Result<MonthReport> {
        let start_balance = self.dao.start_balance(month).await?;
        let bills = self.bills.list_in_month(month).await?;
        let collection = BillMonthCollection::from_bills(month, start_balance, &bills);

        let mut book_names = HashMap::new();
        for book in self.books.list().await? {
            book_names.insert(book.id, book.name);
        }

        let mut category_names = HashMap::new();
        for category in self.categories.list().await? {
            category_names.insert(category.id, category.name);
        }

        Ok(MonthReport {
            collection,
            book_names,
            category_names,
        })
    }

    /// 生成某年报表（按账本 + 逐月走势）。
    pub async fn year_report(&self, year: i32) -> Result<YearReport> {
        let bills = self.bills.list_in_year(year).await?;
        let collection = YearCollection::from_bills(year, &bills);

        let mut book_names = HashMap::new();
        for book in self.books.list().await? {
            book_names.insert(book.id, book.name);
        }

        Ok(YearReport {
            collection,
            book_names,
        })
    }
}
