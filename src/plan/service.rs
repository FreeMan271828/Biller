//! 计划的业务层：一个计划 = 一个账本 + 可选额度 + 类型（收入 / 开销）。
//!
//! 计划本体落在 `bill_books`（`plan_limit` / `plan_kind` 列）：收入计划累计这个月
//! 进账多少，开销计划累计花掉多少——口径都由 `bills.book_id` 天然表达，不需要第二套绑定。
//!
//! 跨模块只认这一层：TUI / CLI 都通过 [`PlanService`] 拿计划、进度与覆盖。

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};

use crate::bill::service::BillService;
use crate::bill_book::model::{Plan, PlanKind};
use crate::bill_book::service::BookService;
use crate::common::db::Pool;
use crate::common::money;
use crate::common::state::StateStore;
use crate::common::types::{AmountType, BookId};
use crate::plan::model::{
    PlanCoverage, PlanProgress, PlanRow, calendar_books, calendar_limit, expense_by_day,
    expense_by_plan, income_by_plan, total_income, total_limit, uncapped_spent,
};

/// 老版本把整份计划存成一行 JSON（`[{name, amount, category?}]`）；迁移完删掉。
const LEGACY_KEY_ITEMS: &str = "plan.items";
/// 更老的标量键，只读用于迁移。
const LEGACY_KEY_EXPECTED: &str = "plan.expected";
/// 更老的标量键，只读用于迁移。
const LEGACY_KEY_SAVINGS: &str = "plan.savings";
/// 迁移只做一次：标记留着，之后新建的空计划不会被误判成「还没迁移」。
const KEY_MIGRATED: &str = "plan.migrated";
/// 老版本「单笔账单计不计入日历」→ 计划级标志，只迁一次。
const KEY_CALENDAR_MIGRATED: &str = "plan.calendar_migrated";

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
    /// 所有计划（含没设额度的）在本月的执行情况，收入计划在前。
    pub rows: Vec<PlanRow>,
    /// 当月额度与日历状态；额度取所有有额度的开销计划之和。
    pub progress: PlanProgress,
    /// 收入计划目标合计 vs 开销额度合计（「还剩」再扣掉没额度计划的已花）。
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

    /// 新建计划；`limit = None` 表示只累计、不设额度。
    pub async fn add(&self, name: &str, limit: Option<AmountType>, kind: PlanKind) -> Result<Plan> {
        let limit = clean_limit(limit)?;
        let book = self.books.add(name, limit, kind).await?;
        Ok(Plan::new(book, limit, kind))
    }

    /// 改某个计划的额度；`None` 表示取消额度。类型与日历标志保持不变。
    pub async fn set_limit(&self, target: &str, limit: Option<AmountType>) -> Result<Plan> {
        let limit = clean_limit(limit)?;
        let book = self.books.find_target(target).await?;
        let current = self.by_id(book.id).await?;
        self.books.set_limit(book.id, limit).await?;
        Ok(Plan::new(book, limit, current.kind).with_calendar(current.in_calendar))
    }

    /// 改类型（收入 / 开销）；额度与日历标志原样保留。
    pub async fn set_kind(&self, target: &str, kind: PlanKind) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        let plan = self.by_id(book.id).await?;
        self.books.set_kind(book.id, kind).await?;
        Ok(Plan::new(book, plan.limit, kind).with_calendar(plan.in_calendar))
    }

    /// 改「是否计入日历」；额度与类型都不动。
    pub async fn set_in_calendar(&self, target: &str, in_calendar: bool) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        let plan = self.by_id(book.id).await?;
        self.books.set_in_calendar(book.id, in_calendar).await?;
        Ok(Plan::new(book, plan.limit, plan.kind).with_calendar(in_calendar))
    }

    /// 改名。计划靠名字找，所以重名会被拒。
    pub async fn rename(&self, target: &str, new_name: &str) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        let plan = self.by_id(book.id).await?;
        let book = self.books.rename(book.id, new_name).await?;
        Ok(Plan::new(book, plan.limit, plan.kind).with_calendar(plan.in_calendar))
    }

    /// 某个计划名下有多少账单：删除前给确认框报数。
    pub async fn bill_count(&self, id: BookId) -> Result<i64> {
        self.books.count_bills(id).await
    }

    /// 删计划：名下账单先整体搬到 `fallback`，再删。返回 (计划, 搬走的账单数)。
    ///
    /// 「默认」计划自己不能当搬运目标——它的账单没处可移，只能先手动改到别的计划。
    pub async fn remove_moving_bills(&self, target: &str, fallback: &str) -> Result<(Plan, i64)> {
        let book = self.books.find_target(target).await?;
        if book.name == fallback {
            bail!("「{fallback}」是默认计划，名下账单没处可移，先改到别的计划再删");
        }
        let plan = self.by_id(book.id).await?;
        let moved = self.books.count_bills(book.id).await?;
        if moved > 0 {
            let fallback_book = self.books.resolve_or_create(fallback).await?.0;
            self.books.move_bills(book.id, fallback_book.id).await?;
        }
        self.books.remove(&book.id.to_string()).await?;
        Ok((plan, moved))
    }

    /// 删掉计划；名下还有账单时数据库会拒绝（外键 RESTRICT），这里翻译成人话。
    pub async fn remove(&self, target: &str) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        let plan = self.by_id(book.id).await?;
        self.books
            .remove(&book.id.to_string())
            .await
            .context("这个计划下还有账单，先把它们改到别的计划或删掉")?;
        Ok(plan)
    }

    /// 按「id 或名称」取某个计划（含额度与类型），CLI 只改类型时用。
    pub async fn get(&self, target: &str) -> Result<Plan> {
        let book = self.books.find_target(target).await?;
        self.by_id(book.id).await
    }

    /// 按 id 取某个计划（含额度与类型）；TUI 改计划时先按 id 拿到「改之前」的样子。
    pub(crate) async fn by_id(&self, id: BookId) -> Result<Plan> {
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
    ///
    /// 覆盖校验的「收入」取**收入计划的目标合计**，不是本月实际收入账单：
    /// 月初还没记工资时，计划页也得能回答「这些开销额度分得完吗」。
    pub async fn snapshot(
        &self,
        month: NaiveDate,
        today: Option<NaiveDate>,
    ) -> Result<PlanSnapshot> {
        let month = first_of_month(month);
        let plans = self.list().await?;
        let bills = self.bills.list_in_month(month).await?;

        let rows = build_rows(plans, &bills, month);
        let income = total_income(&rows);
        // 日历只认「计入日历」的计划：额度与支出两个口径要一致
        let calendar = calendar_books(&rows);
        let spending = expense_by_day(month, &bills, &calendar);
        let limit = calendar_limit(&rows);
        let progress = PlanProgress::build(month, limit, today, &spending);
        // 覆盖校验看全部计划：不计入日历只是不进日历，不等于这笔钱没被分出去
        let coverage = PlanCoverage::new(income, total_limit(&rows), uncapped_spent(&rows));

        Ok(PlanSnapshot {
            rows,
            progress,
            coverage,
        })
    }

    /// 老版本是「单笔账单计不计入」；新版本按计划。
    ///
    /// 迁移规则：一个计划名下的账单**全部**被标成不计入时，把这个计划整体标成不计入；
    /// 混合的保留计入（否则会把本来要算的钱也一起关掉）。只迁一次。
    pub async fn migrate_calendar_flags(&self) -> Result<usize> {
        if self.state.get(KEY_CALENDAR_MIGRATED).await?.is_some() {
            return Ok(0);
        }

        let mut changed = 0;
        for plan in self.list().await? {
            let total = self.books.count_bills(plan.id()).await?;
            if total == 0 {
                continue;
            }
            let excluded = self.bills.count_legacy_excluded(plan.id()).await?;
            if excluded == total && plan.in_calendar {
                self.books.set_in_calendar(plan.id(), false).await?;
                changed += 1;
            }
        }

        self.state.set(KEY_CALENDAR_MIGRATED, "1").await?;
        Ok(changed)
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
                    // 迁移出来的一律是开销计划：老版本没有「收入计划」这个概念
                    self.books.add(name, limit, PlanKind::Expense).await?;
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

/// 额度只接受正数；0 或负数当作「没额度」，免得出现除零和「额度 0」的怪状态。
fn clean_limit(limit: Option<AmountType>) -> Result<Option<AmountType>> {
    match limit {
        Some(value) if value < 0 => bail!("额度不能是负数"),
        Some(0) | None => Ok(None),
        Some(value) => Ok(Some(value)),
    }
}

/// 计划列表 + 当月账单 → 每个计划一行，**收入计划在前、开销计划在后**。
///
/// 两种类型各取一份汇总表（支出过滤「不计入」，收入不过滤），计划按自己的类型挑表。
fn build_rows(
    plans: Vec<Plan>,
    bills: &[crate::bill::model::BillEntity],
    month: NaiveDate,
) -> Vec<PlanRow> {
    let month = first_of_month(month);
    let expenses = expense_by_plan(month, bills);
    let incomes = income_by_plan(month, bills);
    let mut rows: Vec<PlanRow> = plans
        .into_iter()
        .map(|plan| {
            let id = plan.id();
            let by_plan = match plan.kind {
                PlanKind::Income => &incomes,
                PlanKind::Expense => &expenses,
            };
            let spent = by_plan.get(&id).copied().unwrap_or(0);
            PlanRow::new(plan, spent)
        })
        .collect();
    // 先收入后开销；stable sort 保住组内原来的 id 顺序。
    rows.sort_by_key(|row| row.plan.kind != PlanKind::Income);
    rows
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
        assert!(
            plans.iter().all(|plan| plan.kind == PlanKind::Expense),
            "老数据没有收入计划，迁移出来的一律是开销计划"
        );
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

    /// 老版本把「不计入日历」记在单笔账单上：整块都被排除的计划，迁移时整块关掉，且只迁一次。
    #[tokio::test]
    async fn legacy_bill_exclusions_become_a_plan_flag_once() {
        use crate::bill::dao::BillDao;
        use crate::bill::model::BillKind;
        use crate::bill::service::{BillService, NewBill};
        use chrono::{TimeZone, Utc};

        let (pool, dir) = setup().await;
        let plans = PlanService::new(pool.clone());
        let bills = BillService::new(pool.clone());
        let bill_dao = BillDao::new(pool.clone());

        plans
            .add("报销", Some(1_000_00), PlanKind::Expense)
            .await
            .unwrap();
        let (bill, _) = bills
            .add(NewBill {
                kind: BillKind::Expense,
                amount: 300_00,
                book: "报销".into(),
                category: "默认".into(),
                created_at: Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap(),
                remark: None,
            })
            .await
            .unwrap();
        // 老版本的单笔排除：整个计划名下只有这一笔，且被排除
        bill_dao.set_excluded(bill.id, true).await.unwrap();

        assert_eq!(
            plans.migrate_calendar_flags().await.unwrap(),
            1,
            "整块都被排除 → 计划整体关掉"
        );
        assert!(
            !plans.get("报销").await.unwrap().in_calendar,
            "迁移后计划跟着不计入日历"
        );
        assert_eq!(
            plans.migrate_calendar_flags().await.unwrap(),
            0,
            "迁移只做一次"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `plan_kind` 必须真的落在库里：建的时候是收入，读回来还得是收入；
    /// 改额度不能把类型带走，改类型也不能动额度。
    #[tokio::test]
    async fn plan_kind_survives_a_round_trip() {
        let (pool, dir) = setup().await;
        let service = PlanService::new(pool.clone());

        // 故意先建开销、后建收入：库里 id 顺序是「房租 → 工资」
        let expense = service
            .add("房租", Some(2_500_00), PlanKind::Expense)
            .await
            .unwrap();
        let income = service
            .add("工资", Some(8_000_00), PlanKind::Income)
            .await
            .unwrap();
        assert_eq!(expense.kind, PlanKind::Expense, "建的时候是开销计划");
        assert_eq!(income.kind, PlanKind::Income, "建的时候是收入计划");

        let stored = service.get("工资").await.unwrap();
        assert_eq!(stored.kind, PlanKind::Income, "读回来还得是收入计划");
        assert_eq!(stored.limit, Some(8_000_00), "额度也要一起存住");

        // 展示顺序：收入在前、开销在后，和 id 顺序无关
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).expect("2026-10");
        let names: Vec<String> = service
            .rows(month)
            .await
            .unwrap()
            .iter()
            .map(|row| row.plan.name().to_string())
            .collect();
        assert_eq!(names, vec!["工资", "房租"], "先收入后开销");

        // 改额度不掉类型
        let changed = service.set_limit("工资", Some(9_000_00)).await.unwrap();
        assert_eq!(changed.kind, PlanKind::Income, "改额度不该改类型");
        assert_eq!(
            service.get("工资").await.unwrap().kind,
            PlanKind::Income,
            "改额度后从库里读出来也还是收入计划"
        );

        // 改类型不动额度
        let flipped = service.set_kind("工资", PlanKind::Expense).await.unwrap();
        assert_eq!(flipped.kind, PlanKind::Expense);
        assert_eq!(flipped.limit, Some(9_000_00), "改类型不该动额度");
        assert_eq!(
            service.get("工资").await.unwrap().limit,
            Some(9_000_00),
            "改类型后额度还在库里"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 覆盖校验不能把「不计入日历」的计划漏掉：关掉的是支出统计，不是额度分配。
    #[tokio::test]
    async fn coverage_counts_plans_out_of_calendar() {
        let (pool, dir) = setup().await;
        let service = PlanService::new(pool.clone());

        service
            .add("工资", Some(8_000_00), PlanKind::Income)
            .await
            .unwrap();
        service
            .add("房租", Some(2_500_00), PlanKind::Expense)
            .await
            .unwrap();
        service
            .add("报销", Some(1_000_00), PlanKind::Expense)
            .await
            .unwrap();
        service.set_in_calendar("报销", false).await.unwrap();

        let month = NaiveDate::from_ymd_opt(2026, 10, 1).expect("2026-10");
        let snapshot = service.snapshot(month, None).await.unwrap();

        assert_eq!(
            snapshot.coverage.allocated, 3_500_00,
            "不计入日历的额度也要算进覆盖校验"
        );
        assert_eq!(snapshot.coverage.remaining(), 4_500_00);
        // 但概览的本月额度 / 日额度只摊计入日历的计划
        assert_eq!(snapshot.progress.limit, 2_500_00, "不计入日历的额度不摊日额度");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 覆盖校验的「收入」来自收入计划的目标合计，不来自当月账单：
    /// 月初一分钱没记，也得能回答「这些开销额度分得完吗」。
    #[tokio::test]
    async fn coverage_uses_income_plan_targets_not_bills() {
        let (pool, dir) = setup().await;
        let service = PlanService::new(pool.clone());

        service
            .add("房租", Some(2_500_00), PlanKind::Expense)
            .await
            .unwrap();
        service
            .add("工资", Some(8_000_00), PlanKind::Income)
            .await
            .unwrap();
        // 没设目标的收入计划不参与合计
        service.add("外快", None, PlanKind::Income).await.unwrap();

        let month = NaiveDate::from_ymd_opt(2026, 10, 1).expect("2026-10");
        let snapshot = service.snapshot(month, None).await.unwrap();

        // 这个月一笔账单都没有，收入照样有数
        assert_eq!(
            snapshot.coverage.income, 8_000_00,
            "收入 = 收入计划目标合计，不是当月账单"
        );
        assert_eq!(snapshot.coverage.allocated, 2_500_00, "额度 = 开销计划额度合计");
        assert_eq!(snapshot.coverage.remaining(), 5_500_00);
        assert!(snapshot.coverage.covers());
        // 日历额度仍然只摊开销计划
        assert_eq!(snapshot.progress.limit, 2_500_00);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删计划带账单：账单整体搬到「默认」计划，计划本身删掉。
    #[tokio::test]
    async fn removing_a_plan_moves_its_bills_to_default() {
        use crate::bill::model::BillKind;
        use crate::bill::service::{BillService, NewBill};
        use chrono::{TimeZone, Utc};

        let (pool, dir) = setup().await;
        let plans = PlanService::new(pool.clone());
        let books = BookService::new(pool.clone());
        let bills = BillService::new(pool.clone());

        plans
            .add("房租", Some(2_500_00), PlanKind::Expense)
            .await
            .unwrap();
        bills
            .add(NewBill {
                kind: BillKind::Expense,
                amount: 500_00,
                book: "房租".into(),
                category: "默认".into(),
                created_at: Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap(),
                remark: None,
            })
            .await
            .unwrap();

        let (plan, moved) = plans.remove_moving_bills("房租", "默认").await.unwrap();
        assert_eq!(plan.name(), "房租");
        assert_eq!(moved, 1, "名下 1 笔账单被搬走");

        // 计划没了，账单还在「默认」计划名下
        assert!(
            plans.list().await.unwrap().iter().all(|p| p.name() != "房租"),
            "计划本身要删掉"
        );
        let default = books.resolve("默认").await.unwrap();
        assert_eq!(
            books.count_bills(default.id).await.unwrap(),
            1,
            "账单不该跟着计划一起消失"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「默认」计划自己的账单没处可移：拒绝，别删出一批无主账单。
    #[tokio::test]
    async fn default_plan_with_bills_is_not_moved_onto_itself() {
        use crate::bill::model::BillKind;
        use crate::bill::service::{BillService, NewBill};
        use chrono::{TimeZone, Utc};

        let (pool, dir) = setup().await;
        let plans = PlanService::new(pool.clone());
        let bills = BillService::new(pool.clone());

        bills
            .add(NewBill {
                kind: BillKind::Expense,
                amount: 100_00,
                book: "默认".into(),
                category: "默认".into(),
                created_at: Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap(),
                remark: None,
            })
            .await
            .unwrap();

        let error = plans.remove_moving_bills("默认", "默认").await.unwrap_err();
        assert!(
            format!("{error:#}").contains("默认计划"),
            "要说清为什么删不了：{error:#}"
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
