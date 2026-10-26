//! 计划的业务层：一个计划 = 一个账本 + 可选上限。
//!
//! 计划本体落在 `bill_books`（`plan_limit` 列），本月「已花」按账单所属的计划
//! 汇总——「账单统合到计划」这件事由 `bills.book_id` 天然表达，不需要第二套绑定。
//!
//! 跨模块只认这一层：TUI / CLI 都通过 [`PlanService`] 拿计划、进度与覆盖。

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};

use crate::bill::service::BillService;
use crate::bill_book::model::Plan;
use crate::bill_book::service::BookService;
use crate::common::db::Pool;
use crate::common::money;
use crate::common::state::StateStore;
use crate::common::types::{AmountType, BookId};
use crate::plan::model::{
    PlanCoverage, PlanProgress, PlanRow, expense_by_day, expense_by_plan, month_income, total_limit,
};

/// 老版本把整份计划存成一行 JSON（`[{name, amount, category?}]`）；迁移完删掉。
const LEGACY_KEY_ITEMS: &str = "plan.items";
/// 更老的标量键，只读用于迁移。
const LEGACY_KEY_EXPECTED: &str = "plan.expected";
/// 更老的标量键，只读用于迁移。
const LEGACY_KEY_SAVINGS: &str = "plan.savings";
/// 迁移只做一次：标记留着，之后新建的空计划不会被误判成「还没迁移」。
const KEY_MIGRATED: &str = "plan.migrated";

#[derive(Clone)]
pub struct PlanService {
    books: BookService,
    bills: BillService,
    state: StateStore,
}

/// 一次 reload 需要的全部计划数据：列表、当月执行、覆盖情况。
///
/// 三样都要当月账单，拆成三个方法就得查三遍，干脆一次算完。
pub struct PlanSnapshot {
    /// 所有计划（含没设上限的）在本月的执行情况。
    pub rows: Vec<PlanRow>,
    /// 当月额度与日历状态；上限取所有有上限计划之和。
    pub progress: PlanProgress,
    /// 计划合计 vs 当月收入。
    pub coverage: PlanCoverage,
}

impl PlanService {
    pub fn new(pool: Pool) -> Self {
        Self {
            books: BookService::new(pool.clone()),
            bills: BillService::new(pool.clone()),
            state: StateStore::new(pool),
        }
    }

    /// 所有计划（账本表按名称升序）。
    pub async fn list(&self) -> Result<Vec<Plan>> {
        self.books.list_plans().await
    }

    /// 新建计划；`limit = None` 表示只累计、不设上限。
    pub async fn add(&self, name: &str, limit: Option<AmountType>) -> Result<Plan> {
        let limit = clean_limit(limit)?;
        let book = self.books.add(name, limit).await?;
        Ok(Plan::new(book, limit))
    }

    /// 改某个计划的上限；`None` 表示取消上限。
    pub async fn set_limit(&self, target: &str, limit: Option<AmountType>) -> Result<Plan> {
        let limit = clean_limit(limit)?;
        let book = self.books.find_target(target).await?;
        self.books.set_limit(book.id, limit).await?;
        Ok(Plan::new(book, limit))
    }

    /// 改名。计划靠名字找，所以重名会被拒。
    pub async fn rename(&self, target: &str, new_name: &str) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        let limit = self.plan_of(book.id).await?.limit;
        let book = self.books.rename(book.id, new_name).await?;
        Ok(Plan::new(book, limit))
    }

    /// 删掉计划；名下还有账单时数据库会拒绝（外键 RESTRICT），这里翻译成人话。
    pub async fn remove(&self, target: &str) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        let plan = self.plan_of(book.id).await?;
        self.books
            .remove(&book.id.to_string())
            .await
            .context("这个计划下还有账单，先把它们改到别的计划或删掉")?;
        Ok(plan)
    }

    /// 某个计划（含上限）。
    async fn plan_of(&self, id: BookId) -> Result<Plan> {
        self.list()
            .await?
            .into_iter()
            .find(|plan| plan.id() == id)
            .with_context(|| format!("计划 id={id} 不存在"))
    }

    /// 所有计划在本月的执行情况：本月账单按计划分组，再跟计划列表对齐。
    pub async fn rows(&self, month: NaiveDate) -> Result<Vec<PlanRow>> {
        let plans = self.list().await?;
        let bills = self.bills.list_in_month(first_of_month(month)).await?;
        Ok(build_rows(plans, &bills, month))
    }

    /// TUI 每月的 reload 走这里：一次查询把列表、日历、覆盖都算好。
    pub async fn snapshot(
        &self,
        month: NaiveDate,
        today: Option<NaiveDate>,
    ) -> Result<PlanSnapshot> {
        let month = first_of_month(month);
        let plans = self.list().await?;
        let bills = self.bills.list_in_month(month).await?;

        let rows = build_rows(plans, &bills, month);
        let limit = total_limit(&rows);
        let spending = expense_by_day(month, &bills);
        let progress = PlanProgress::build(month, limit, today, &spending);
        let coverage = PlanCoverage::new(month_income(month, &bills), limit);

        Ok(PlanSnapshot {
            rows,
            progress,
            coverage,
        })
    }

    /// 把老版本那行 JSON 里的小块搬成「带上限的计划」，只做一次。
    ///
    /// 同名的计划已经存在就只把上限补上，不重复建；返回新建了几个。
    pub async fn migrate_items_into_plans(&self) -> Result<usize> {
        if self.state.get(KEY_MIGRATED).await?.is_some() {
            return Ok(0);
        }

        let items = self.legacy_items().await?;
        let mut created = 0;
        for (name, amount) in items {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let limit = (amount > 0).then_some(amount);
            match self.books.resolve(name).await {
                Ok(book) => {
                    if limit.is_some() {
                        self.books.set_limit(book.id, limit).await?;
                    }
                }
                Err(_) => {
                    self.books.add(name, limit).await?;
                    created += 1;
                }
            }
        }

        self.clear_legacy().await?;
        self.state.set(KEY_MIGRATED, "1").await?;
        Ok(created)
    }

    /// 老数据读一遍：优先读 JSON，其次把更老的「预计 / 攒钱」两个标量拼成两块。
    async fn legacy_items(&self) -> Result<Vec<(String, AmountType)>> {
        if let Some(raw) = self.state.get(LEGACY_KEY_ITEMS).await?
            && let Some(items) = decode_items(&raw)
        {
            return Ok(items);
        }

        let expected = self.legacy_amount(LEGACY_KEY_EXPECTED).await?;
        let savings = self.legacy_amount(LEGACY_KEY_SAVINGS).await?;
        Ok(scalars_to_items(expected, savings))
    }

    async fn legacy_amount(&self, key: &str) -> Result<AmountType> {
        let Some(raw) = self.state.get(key).await? else {
            return Ok(0);
        };
        let raw = raw.trim();
        if let Ok(value) = raw.parse::<AmountType>() {
            return Ok(value);
        }
        Ok(money::parse_amount(raw).unwrap_or(0))
    }

    async fn clear_legacy(&self) -> Result<()> {
        for key in [LEGACY_KEY_ITEMS, LEGACY_KEY_EXPECTED, LEGACY_KEY_SAVINGS] {
            self.state.remove(key).await?;
        }
        Ok(())
    }
}

/// 上限只接受正数；0 或负数当作「没上限」，免得出现除零和「额度 0」的怪状态。
fn clean_limit(limit: Option<AmountType>) -> Result<Option<AmountType>> {
    match limit {
        Some(value) if value < 0 => bail!("上限不能是负数"),
        Some(0) | None => Ok(None),
        Some(value) => Ok(Some(value)),
    }
}

/// 计划列表 + 当月账单 → 每个计划一行。
fn build_rows(
    plans: Vec<Plan>,
    bills: &[crate::bill::model::BillEntity],
    month: NaiveDate,
) -> Vec<PlanRow> {
    let by_plan = expense_by_plan(first_of_month(month), bills);
    plans
        .into_iter()
        .map(|plan| {
            let spent = by_plan.get(&plan.id()).copied().unwrap_or(0);
            PlanRow::new(plan, spent)
        })
        .collect()
}

/// 月份统一成 1 号：库里的月份键、账单查询都按 1 号算。
fn first_of_month(month: NaiveDate) -> NaiveDate {
    month.with_day(1).unwrap_or(month)
}

/// JSON → `[(名字, 金额)]`；格式不认识就当没存过。
///
/// 两种历史格式都认：
///
/// * 1.2.0 的 `[[name, amount], ...]`（二元组数组，没有分类）
/// * 后来的 `[{name, amount, category?}, ...]`
///
/// 分类绑定随「账单统合到计划」一起退场，读的时候直接忽略。
fn decode_items(raw: &str) -> Option<Vec<(String, AmountType)>> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let array = value.as_array()?;

    let mut items = Vec::with_capacity(array.len());
    for entry in array {
        match entry {
            serde_json::Value::Object(object) => {
                let name = object.get("name")?.as_str()?.to_string();
                let amount = object.get("amount")?.as_i64()?;
                items.push((name, amount));
            }
            serde_json::Value::Array(pair) if pair.len() == 2 => {
                items.push((pair[0].as_str()?.to_string(), pair[1].as_i64()?));
            }
            _ => return None,
        }
    }
    Some(items)
}

/// 旧版「预计 / 攒钱」→ 分块计划。
///
/// 旧语义里 `预计 - 攒钱` 是当月可花的钱，攒钱那部分不花：
/// 于是落到两块——「日常开销」拿可花的那部分，「存款」拿攒下的那部分。
fn scalars_to_items(expected: AmountType, savings: AmountType) -> Vec<(String, AmountType)> {
    if expected <= 0 && savings <= 0 {
        return Vec::new();
    }

    let mut items = Vec::new();
    let spendable = (expected - savings).max(0);
    if spendable > 0 {
        items.push(("日常开销".to_string(), spendable));
    }
    if savings > 0 {
        items.push(("存款".to_string(), savings));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_understands_both_historical_shapes() {
        let modern = decode_items(r#"[{"name":"日常开销","amount":300000,"category":"餐饮"}]"#)
            .expect("新格式");
        assert_eq!(modern, vec![("日常开销".to_string(), 300_000)]);

        let legacy = decode_items(r#"[["存款",900000]]"#).expect("旧格式");
        assert_eq!(legacy, vec![("存款".to_string(), 900_000)]);
    }

    #[test]
    fn decode_gives_up_on_unknown_json() {
        assert!(decode_items("不是 JSON").is_none());
        assert!(decode_items(r#"{"name":"x"}"#).is_none());
    }

    #[test]
    fn scalar_plan_splits_into_spending_and_savings() {
        assert_eq!(
            scalars_to_items(1_000_00, 300_00),
            vec![
                ("日常开销".to_string(), 700_00),
                ("存款".to_string(), 300_00)
            ]
        );
        assert_eq!(
            scalars_to_items(1_000_00, 0),
            vec![("日常开销".to_string(), 1_000_00)]
        );
        assert!(scalars_to_items(0, 0).is_empty());
    }

    use crate::bill_book::service::BookService;
    use crate::common::db;
    use crate::common::state::StateStore;

    /// 临时 SQLite 文件 + 建表；返回池与目录（测试结束删目录）。
    async fn setup() -> (Pool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "biller-plan-svc-{}-{:?}",
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

    /// 1.3 的 plan.items 是「若干块 JSON」；打开 1.4 时这些块要变成计划，且只搬一次。
    #[tokio::test]
    async fn legacy_items_become_plans_once() {
        let (pool, dir) = setup().await;
        let service = PlanService::new(pool.clone());
        let state = StateStore::new(pool.clone());

        state
            .set(
                LEGACY_KEY_ITEMS,
                r#"[{"name":"日常开销","amount":400000},{"name":"存款","amount":100000}]"#,
            )
            .await
            .unwrap();

        assert_eq!(service.migrate_items_into_plans().await.unwrap(), 2);

        let plans = BookService::new(pool.clone()).list_plans().await.unwrap();
        let names: Vec<&str> = plans.iter().map(|plan| plan.name()).collect();
        assert_eq!(names, vec!["日常开销", "存款"]);
        assert_eq!(plans[0].limit, Some(400_000), "块的面额就是计划上限");
        assert!(state.get(LEGACY_KEY_ITEMS).await.unwrap().is_none(), "旧键要清掉");

        // 再开一次界面不该翻倍
        assert_eq!(service.migrate_items_into_plans().await.unwrap(), 0);
        assert_eq!(
            BookService::new(pool.clone())
                .list_plans()
                .await
                .unwrap()
                .len(),
            2
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_or_negative_limit_means_uncapped() {
        assert_eq!(clean_limit(Some(0)).expect("可以用"), None);
        assert_eq!(clean_limit(None).expect("可以用"), None);
        assert_eq!(clean_limit(Some(500_00)).expect("可以用"), Some(500_00));
        assert!(clean_limit(Some(-1)).is_err());
    }
}
