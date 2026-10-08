//! 账单业务规则。
//!
//! 跨领域只依赖对方的 service：账本走 [`BookService`]，分类走 [`CategoryService`]。

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::bill::dao::{BillDao, BillFilter};
use crate::bill::model::{BillEntity, BillKind};
use crate::bill_book::service::BookService;
use crate::category::service::CategoryService;
use crate::common::types::{AmountType, BillId};

/// 新增账单的入参：账本/分类用名称表达，由 service 负责解析。
#[derive(Debug, Clone)]
pub struct NewBill {
    pub kind: BillKind,
    pub amount: AmountType,
    pub book: String,
    pub category: String,
    pub created_at: DateTime<Utc>,
    pub remark: Option<String>,
}

/// 账单查询入参：账本/分类用名称表达，由 service 负责解析。
#[derive(Debug, Clone, Default)]
pub struct BillQuery {
    pub month: Option<NaiveDate>,
    pub book: Option<String>,
    pub category: Option<String>,
    pub kind: Option<BillKind>,
    pub limit: Option<i64>,
}

use crate::common::state::StateStore;

/// `app_state` 里存「上次记账用的日期」的键。
const KEY_LAST_USED_DATE: &str = "bill.last_used_date";
/// `app_state` 里存「上次记账用的账本名」的键。
const KEY_LAST_USED_BOOK: &str = "bill.last_used_book";

#[derive(Clone)]
pub struct BillService {
    dao: BillDao,
    books: BookService,
    categories: CategoryService,
    state: StateStore,
}

impl BillService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            dao: BillDao::new(pool.clone()),
            books: BookService::new(pool.clone()),
            categories: CategoryService::new(pool.clone()),
            state: StateStore::new(pool),
        }
    }

    /// 上次记账用过的日期；从未记过则返回 `None`（新增表单留空即今天）。
    pub async fn last_used_date(&self) -> Result<Option<NaiveDate>> {
        match self.state.get(KEY_LAST_USED_DATE).await? {
            Some(text) => Ok(NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()),
            None => Ok(None),
        }
    }

    /// 记下这次用的日期，供下次新增账单时预填。
    pub async fn remember_entry_date(&self, date: NaiveDate) -> Result<()> {
        self.state.set(KEY_LAST_USED_DATE, &date.to_string()).await
    }

    /// 上次记账用的账本名：下次新增账单时默认选中它。
    ///
    /// 只返回名字；该账本后来被删掉时由调用方回落到列表里的第一个。
    pub async fn default_entry_book(&self) -> Result<Option<String>> {
        Ok(self
            .state
            .get(KEY_LAST_USED_BOOK)
            .await?
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()))
    }

    /// 记下这次用的账本，供下次新增账单时默认选中。
    pub async fn remember_entry_book(&self, name: &str) -> Result<()> {
        self.state.set(KEY_LAST_USED_BOOK, name).await
    }

    /// 记一笔账。
    ///
    /// 返回账单本体，以及本次自动创建了哪些账本/分类（供终端提示）。
    pub async fn add(&self, new: NewBill) -> Result<(BillEntity, Vec<String>)> {
        if new.amount <= 0 {
            bail!("金额必须大于 0（收入/支出方向由 kind 决定）");
        }

        let (book, book_created) = self.books.resolve_or_create(&new.book).await?;
        let (category, category_created) = self.categories.resolve_or_create(&new.category).await?;

        let id = self
            .dao
            .insert(
                new.kind,
                new.amount,
                book.id,
                category.id,
                new.created_at,
                new.remark.as_deref(),
            )
            .await?;

        // 记住这次用的日期与账本，下次新增账单默认沿用
        self.remember_entry_date(new.created_at.date_naive())
            .await?;
        self.remember_entry_book(&book.name).await?;

        let mut auto_created = Vec::new();
        if book_created {
            auto_created.push(format!("账本「{}」", book.name));
        }
        if category_created {
            auto_created.push(format!("分类「{}」", category.name));
        }

        Ok((
            BillEntity {
                id,
                kind: new.kind,
                amount: new.amount,
                book,
                category,
                created_at: new.created_at,
                remark: new.remark,
                // 新账单默认纳入统计
                excluded: false,
            },
            auto_created,
        ))
    }

    /// 修改一笔账单（整体覆盖，未变的字段由调用方沿用原值）。
    pub async fn update(&self, id: BillId, new: NewBill) -> Result<BillEntity> {
        if new.amount <= 0 {
            bail!("金额必须大于 0（收入/支出方向由 kind 决定）");
        }

        let (book, _) = self.books.resolve_or_create(&new.book).await?;
        let (category, _) = self.categories.resolve_or_create(&new.category).await?;

        let affected = self
            .dao
            .update(
                id,
                new.kind,
                new.amount,
                book.id,
                category.id,
                new.created_at,
                new.remark.as_deref(),
            )
            .await?;
        if affected == 0 {
            bail!("账单 id={id} 不存在");
        }

        // 编辑账单不动「是否纳入统计」：取原值带上，免得返回值误导调用方
        let excluded = self
            .dao
            .get(id)
            .await?
            .map(|bill| bill.excluded)
            .unwrap_or(false);

        Ok(BillEntity {
            id,
            kind: new.kind,
            amount: new.amount,
            book,
            category,
            created_at: new.created_at,
            remark: new.remark,
            excluded,
        })
    }

    /// 取单条账单，供编辑时预填表单。
    pub async fn get(&self, id: BillId) -> Result<BillEntity> {
        self.dao
            .get(id)
            .await?
            .with_context(|| format!("账单 id={id} 不存在"))
    }

    pub async fn list(&self, query: BillQuery) -> Result<Vec<BillEntity>> {
        let filter = self.to_filter(query).await?;
        self.dao.list(&filter).await
    }

    /// 供月度领域调用：取某月全部账单。
    pub async fn list_in_month(&self, month: NaiveDate) -> Result<Vec<BillEntity>> {
        self.dao
            .list(&BillFilter {
                month: Some(month),
                ..Default::default()
            })
            .await
    }

    /// 供年度报表调用：取某年全部账单。
    pub async fn list_in_year(&self, year: i32) -> Result<Vec<BillEntity>> {
        self.dao
            .list(&BillFilter {
                year: Some(year),
                ..Default::default()
            })
            .await
    }

    pub async fn remove(&self, id: BillId) -> Result<()> {
        if self.dao.delete(id).await? == 0 {
            bail!("账单 id={id} 不存在");
        }
        Ok(())
    }

    /// 切换一条账单「是否纳入统计」。
    ///
    /// 不纳入统计的账单仍然存在、仍能在列表里看到，只是不参与收支汇总与开销计划。
    pub async fn set_excluded(&self, id: BillId, excluded: bool) -> Result<()> {
        if self.dao.set_excluded(id, excluded).await? == 0 {
            bail!("账单 id={id} 不存在");
        }
        Ok(())
    }

    /// 把「名称」条件解析成 id，再交给 dao。
    async fn to_filter(&self, query: BillQuery) -> Result<BillFilter> {
        let book_id = match non_empty(query.book.as_deref()) {
            Some(name) => Some(self.books.resolve(name).await?.id),
            None => None,
        };
        let category_id = match non_empty(query.category.as_deref()) {
            Some(name) => Some(self.categories.resolve(name).await?.id),
            None => None,
        };

        Ok(BillFilter {
            month: query.month,
            // 年度查询走 list_in_year，经由 BillQuery 的筛选不带 year
            year: None,
            book_id,
            category_id,
            kind: query.kind,
            limit: query.limit,
        })
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}
