//! 账单业务规则。
//!
//! 跨领域只依赖对方的 service：账本走 [`BookService`]，分类走 [`CategoryService`]。

use crate::common::db::{Pool, Tx};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};

use crate::bill::dao::{BillDao, BillFilter};
use crate::bill::model::{BillEntity, BillKind};
use crate::bill_book::model::BillBook;
use crate::bill_book::service::BookService;
use crate::category::model::Category;
use crate::category::service::CategoryService;
use crate::common::types::{AmountType, BillId};

/// 没记账本 / 分类时落到哪：这两个名字在库里不存在就自动创建。
///
/// 放在账单一侧而不是银行一侧，因为「新增账单默认落到默认账本/分类」
/// 是本模块的规则，银行同步只是恰好也用它。
pub const DEFAULT_BOOK: &str = "默认";
pub const DEFAULT_CATEGORY: &str = "默认";

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
/// `app_state` 里存「上次记账用的分类名」的键。
const KEY_LAST_USED_CATEGORY: &str = "bill.last_used_category";

#[derive(Clone)]
pub struct BillService {
    dao: BillDao,
    books: BookService,
    categories: CategoryService,
    state: StateStore,
}

impl BillService {
    pub fn new(pool: Pool) -> Self {
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

    /// 上次记账用的分类名：下次新增账单时默认选中它。
    ///
    /// 与账本一样只返回名字；分类被删掉时由调用方回落到列表里的第一个。
    pub async fn default_entry_category(&self) -> Result<Option<String>> {
        Ok(self
            .state
            .get(KEY_LAST_USED_CATEGORY)
            .await?
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()))
    }

    /// 记下这次用的分类，供下次新增账单时默认选中。
    pub async fn remember_entry_category(&self, name: &str) -> Result<()> {
        self.state.set(KEY_LAST_USED_CATEGORY, name).await
    }

    /// 记一笔账。
    ///
    /// 返回账单本体，以及本次自动创建了哪些账本/分类（供终端提示）。
    pub async fn add(&self, new: NewBill) -> Result<(BillEntity, Vec<String>)> {
        let (book, category, auto_created) = self.prepare_entry(&new).await?;

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

        // 记住这次用的日期、账本与分类，下次新增账单默认沿用
        self.remember_entry_date(new.created_at.date_naive())
            .await?;
        self.remember_entry_book(&book.name).await?;
        self.remember_entry_category(&category.name).await?;

        Ok((entity(id, &new, book, category), auto_created))
    }

    /// 入账前的准备：校验金额，并把「账本 / 分类名称」解析成实体（不存在则自动创建）。
    ///
    /// 银行同步要在**一个事务**里写「账单 + 导入标记 + 记忆」，因此把解析与插入拆开：
    /// 解析可能写库（自动建账本/分类），放在事务外更稳妥；插入走
    /// [`BillService::insert_prepared`] 进入调用方的事务。
    pub async fn prepare_entry(&self, new: &NewBill) -> Result<(BillBook, Category, Vec<String>)> {
        if new.amount <= 0 {
            bail!("金额必须大于 0（收入/支出方向由 kind 决定）");
        }

        let (book, book_created) = self.books.resolve_or_create(&new.book).await?;
        let (category, category_created) = self.categories.resolve_or_create(&new.category).await?;

        let mut auto_created = Vec::new();
        if book_created {
            auto_created.push(format!("账本「{}」", book.name));
        }
        if category_created {
            auto_created.push(format!("分类「{}」", category.name));
        }

        Ok((book, category, auto_created))
    }

    /// 在调用方的事务里插入一笔已解析好账本/分类的账单。
    ///
    /// 与 [`BillService::add`] 的区别：不碰 `app_state`（「上次用的日期/账本」
    /// 由调用方在提交后自行补记），账单插入参与调用方的事务。
    pub async fn insert_prepared(
        &self,
        tx: &mut Tx<'_>,
        new: &NewBill,
        book: BillBook,
        category: Category,
    ) -> Result<BillEntity> {
        if new.amount <= 0 {
            bail!("金额必须大于 0（收入/支出方向由 kind 决定）");
        }

        let id = self
            .dao
            .insert_in(
                tx,
                new.kind,
                new.amount,
                book.id,
                category.id,
                new.created_at,
                new.remark.as_deref(),
            )
            .await?;

        Ok(entity(id, new, book, category))
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
    /// 不纳入统计的账单仍然存在、仍能在列表里看到，只是不参与支出日历的口径。
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

/// 由插入结果拼出账单实体（新账单默认纳入统计）。
fn entity(id: BillId, new: &NewBill, book: BillBook, category: Category) -> BillEntity {
    BillEntity {
        id,
        kind: new.kind,
        amount: new.amount,
        book,
        category,
        created_at: new.created_at,
        remark: new.remark.clone(),
        excluded: false,
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    use crate::common::db;

    /// 临时 SQLite 文件 + 建表；返回池与目录（测试结束删目录）。
    async fn setup() -> (Pool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "biller-bill-svc-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let pool = db::connect(&db::sqlite_url(&dir.join("biller.db")))
            .await
            .expect("连接临时 SQLite");
        db::init_schema(&pool).await.expect("建表");

        (pool, dir)
    }

    fn new_bill(amount: AmountType) -> NewBill {
        NewBill {
            kind: BillKind::Expense,
            amount,
            book: "微信".to_string(),
            category: "餐饮".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap(),
            remark: Some("午饭".to_string()),
        }
    }

    /// 事务内插入：提交才可见，未提交必须整体回滚（银行同步的原子性靠它）。
    #[tokio::test]
    async fn insert_prepared_obeys_the_callers_transaction() {
        let (pool, dir) = setup().await;
        let service = BillService::new(pool.clone());

        // 提交路径
        let input = new_bill(1234);
        let (book, category, _) = service.prepare_entry(&input).await.unwrap();
        let mut tx = pool.any().begin().await.unwrap();
        let bill = service
            .insert_prepared(&mut tx, &input, book, category)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(service.get(bill.id).await.unwrap().amount, 1234);

        // 回滚路径：不提交直接丢弃事务，账单不能留下
        let input = new_bill(999);
        let (book, category, _) = service.prepare_entry(&input).await.unwrap();
        let mut tx = pool.any().begin().await.unwrap();
        let ghost = service
            .insert_prepared(&mut tx, &input, book, category)
            .await
            .unwrap();
        drop(tx);
        assert!(
            service.get(ghost.id).await.is_err(),
            "未提交的账单必须随事务回滚，不能出现「半笔」入账"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 按非 1 号日期筛月份也必须命中（PG 侧是 `= 'YYYY-MM-01'::date`，不归一会查不到）。
    #[tokio::test]
    async fn month_filter_accepts_any_day_of_the_month() {
        let (pool, dir) = setup().await;
        let service = BillService::new(pool.clone());

        let (bill, _) = service.add(new_bill(500)).await.unwrap();

        let by_fifteenth = service
            .list(BillQuery {
                month: Some(NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_fifteenth.len(), 1, "传当月任意一天都应筛出整月");
        assert_eq!(by_fifteenth[0].id, bill.id);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 负数 / 0 的 limit 视为不限条数（两个后端对负 LIMIT 的处理不同，必须归一）。
    #[tokio::test]
    async fn non_positive_limit_means_no_limit() {
        let (pool, dir) = setup().await;
        let service = BillService::new(pool.clone());

        service.add(new_bill(100)).await.unwrap();
        service.add(new_bill(200)).await.unwrap();

        for limit in [-1, 0] {
            let bills = service
                .list(BillQuery {
                    limit: Some(limit),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(bills.len(), 2, "limit={limit} 应视为不限");
        }

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
