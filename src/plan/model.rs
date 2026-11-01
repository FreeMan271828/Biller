//! 计划的读模型：一个计划 = 一个账本 + 可选额度 + 类型（收入 / 开销）。
//!
//! 计划本体存在 `bill_books`（见 [`crate::bill_book::model::Plan`]），这里只放
//! 「把这个月的账单摊到计划上」用的派生结构：谁花了多少、还剩多少，
//! 以及支出日历要的日额度与逐日状态。
//!
//! 两种类型共用一套结构，口径只差一个方向：
//!
//! * **收入计划**（`plan.kind == Income`）：累计这个月进账多少，跟「目标」比，
//!   目标合计就是覆盖校验里的「收入」；「不计入」的账单照样算收入；
//! * **开销计划**（`Expense`）：累计这个月花掉多少，跟「上限」比，能上色日历；
//!   「不计入」的账单不算支出。

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, NaiveDate};

use crate::bill::model::{BillEntity, BillKind};
use crate::bill_book::model::{Plan, PlanKind};
use crate::common::types::{AmountType, BookId};

/// 一个计划在本月的执行情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub plan: Plan,
    /// 本月记在这个计划名下的金额：开销计划是支出合计（「不计入」的账单不算），
    /// 收入计划是收入合计（「不计入」不影响收入口径）。
    pub spent: AmountType,
}

impl PlanRow {
    pub fn new(plan: Plan, spent: AmountType) -> Self {
        Self { plan, spent }
    }

    pub fn name(&self) -> &str {
        self.plan.name()
    }

    /// 还剩多少：只有设了额度才有意义，没额度返回 `None`（不拿它当 0 用）。
    /// 收入计划里它是「离目标还差多少」，负数表示已经超额完成。
    pub fn remaining(&self) -> Option<AmountType> {
        self.plan.limit.map(|limit| limit - self.spent)
    }

    /// 超支了吗：没额度的计划永远不会超。
    ///
    /// 收入计划里 `remaining < 0` 是「超额完成」，不是超支，调用方按类型上色。
    pub fn over(&self) -> bool {
        self.remaining().is_some_and(|rest| rest < 0)
    }

    /// 已用比例；没上限或上限为 0 时给 0，避免除零。
    pub fn ratio(&self) -> f64 {
        match self.plan.limit {
            Some(limit) if limit > 0 => self.spent as f64 / limit as f64,
            _ => 0.0,
        }
    }
}

/// 所有**开销计划**的额度合计：覆盖校验的「开销额度」取这个数（不分进不进日历）。
///
/// 收入计划不参与：覆盖校验比的是「开销额度合计 vs 收入目标」。
/// **不计入日历的计划照样参与**：关掉的是「这笔支出算不算进日历」，
/// 不是「这笔钱有没有被分出去」——覆盖校验回答的是「这些额度分得完吗」，必须看全部计划。
/// 日历 / 概览的「本月额度」要用另一把尺子，见 [`calendar_limit`]。
/// 没额度的计划不参与——没有额度可摊。
pub fn total_limit(rows: &[PlanRow]) -> AmountType {
    rows.iter()
        .filter(|row| row.plan.kind == PlanKind::Expense)
        .filter_map(|row| row.plan.limit)
        .filter(|limit| *limit > 0)
        .sum()
}

/// **计入日历的**开销计划额度合计：概览「本月额度」、日额度、日历进度取这个数。
///
/// 不计入日历的计划不摊额度：它名下的支出本来就不进日历，额度再摊进来
/// 等于凭空多出可用天数，日额度会被算虚。
pub fn calendar_limit(rows: &[PlanRow]) -> AmountType {
    rows.iter()
        .filter(|row| row.plan.kind == PlanKind::Expense && row.plan.in_calendar)
        .filter_map(|row| row.plan.limit)
        .filter(|limit| *limit > 0)
        .sum()
}

/// 计入日历的计划 id 集合：算日历 / 进度时按它筛账单。
///
/// 账单自己没有「计入日历」开关，一律跟着计划走。
pub fn calendar_books(rows: &[PlanRow]) -> HashSet<BookId> {
    rows.iter()
        .filter(|row| row.plan.in_calendar)
        .map(|row| row.plan.id())
        .collect()
}

/// 所有**收入计划**的目标合计：覆盖校验的「收入」取这个数。
///
/// 不拿账单去凑：收入计划是「这个月打算进来多少」，月初还没记账时也得有数，
/// 否则覆盖校验永远是一片空白。没设目标的收入计划不参与——它没有承诺可对照。
pub fn total_income(rows: &[PlanRow]) -> AmountType {
    rows.iter()
        .filter(|row| row.plan.kind == PlanKind::Income)
        .filter_map(|row| row.plan.limit)
        .filter(|limit| *limit > 0)
        .sum()
}

/// 没设额度的开销计划，本月已经花掉多少。
///
/// 覆盖校验的「还剩」必须把它扣掉：没额度不等于没花钱，
/// 否则「还剩」会把已经花出去、只是没预算的那部分钱重新算成可用。
/// 同样不看「是否计入日历」——覆盖校验算的是全部计划。
pub fn uncapped_spent(rows: &[PlanRow]) -> AmountType {
    rows.iter()
        .filter(|row| row.plan.kind == PlanKind::Expense && !row.plan.has_limit())
        .map(|row| row.spent)
        .sum()
}

/// 收入目标 vs 开销额度：这张卡回答「钱分完了吗、分超了吗」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCoverage {
    /// 收入计划的目标合计（不是本月实际收入账单）。
    pub income: AmountType,
    /// 开销计划额度合计。
    pub allocated: AmountType,
    /// 没设额度的开销计划本月已花（从「还剩」里扣掉）。
    pub uncapped_spent: AmountType,
}

impl PlanCoverage {
    pub fn new(income: AmountType, allocated: AmountType, uncapped_spent: AmountType) -> Self {
        Self {
            income,
            allocated,
            uncapped_spent,
        }
    }

    /// 收入目标减开销额度、再减没额度计划的已花：正数=还有没分配的，负数=分超了。
    pub fn remaining(&self) -> AmountType {
        self.income - self.allocated - self.uncapped_spent
    }

    /// 分超的部分（没超就是 0）。
    pub fn shortfall(&self) -> AmountType {
        (-self.remaining()).max(0)
    }

    /// 能不能覆盖。等号算覆盖——刚好分完也是一种答案。
    pub fn covers(&self) -> bool {
        self.remaining() >= 0
    }

    /// 开销额度占收入目标的比例；收入目标为 0 时给 0，避免除零。
    pub fn ratio(&self) -> f64 {
        if self.income <= 0 {
            return 0.0;
        }
        self.allocated as f64 / self.income as f64
    }
}

/// 某月有多少天。
pub fn days_in_month(month: NaiveDate) -> u32 {
    let (year, month_number) = (month.year(), month.month());
    let next = if month_number == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month_number + 1, 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month_number, 1);
    match (first, next) {
        (Some(start), Some(end)) => (end - start).num_days() as u32,
        _ => 30,
    }
}

/// 一天的状态：日历用它上色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayStatus {
    /// 当天没有任何支出（或还没设上限）。
    None,
    /// 当天在日额度以内。
    WithinDaily,
    /// 当天超了日额度，但累计还没超本月上限。
    OverDaily,
    /// 累计已经超过本月上限。
    OverLimit,
}

/// 日历一格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayCell {
    pub day: u32,
    pub spent: AmountType,
    pub status: DayStatus,
}

/// 当月执行情况：把计划上限摊到每一天，再和账单对账。
#[derive(Debug, Clone)]
pub struct PlanProgress {
    pub month: NaiveDate,
    /// 本月上限 = 所有有上限的**开销**计划之和（收入计划是目标不是预算，不摊进日额度）。
    pub limit: AmountType,
    /// 日额度 = 上限 / 当月天数。
    pub daily_allowance: AmountType,
    /// 当月已花（「不计入日历」的支出不算）。
    pub spent: AmountType,
    /// 日均开销：按已经过去的天数算，未来天数不当分母。
    pub daily_average: AmountType,
    pub cells: Vec<DayCell>,
}

impl PlanProgress {
    pub fn build(
        month: NaiveDate,
        limit: AmountType,
        today: Option<NaiveDate>,
        spending_by_day: &HashMap<u32, AmountType>,
    ) -> Self {
        let days = days_in_month(month).max(1);
        let daily_allowance = if limit > 0 { limit / days as AmountType } else { 0 };
        let spent: AmountType = spending_by_day.values().sum();

        // 已经过去的天数：只数到今天，未来的日子不该拉低日均
        let elapsed = today
            .filter(|today| today.year() == month.year() && today.month() == month.month())
            .map(|today| today.day())
            .unwrap_or(days)
            .clamp(1, days);
        let daily_average = spent / elapsed as AmountType;

        let mut cumulative: AmountType = 0;
        let cells = (1..=days)
            .map(|day| {
                let spent_today = spending_by_day.get(&day).copied().unwrap_or(0);
                cumulative += spent_today;
                let status = if limit <= 0 {
                    DayStatus::None
                } else if cumulative > limit {
                    DayStatus::OverLimit
                } else if spent_today > daily_allowance {
                    DayStatus::OverDaily
                } else if spent_today == 0 {
                    DayStatus::None
                } else {
                    DayStatus::WithinDaily
                };
                DayCell {
                    day,
                    spent: spent_today,
                    status,
                }
            })
            .collect();

        Self {
            month,
            limit,
            daily_allowance,
            spent,
            daily_average,
            cells,
        }
    }

    /// 本月累计超过上限了吗。
    pub fn over_limit(&self) -> bool {
        self.limit > 0 && self.spent > self.limit
    }

    /// 本月还剩多少额度（超了就是负数）。
    pub fn remaining(&self) -> AmountType {
        self.limit - self.spent
    }

    /// 已用比例；没设上限时给 0。
    pub fn used_ratio(&self) -> f64 {
        if self.limit <= 0 {
            return 0.0;
        }
        self.spent as f64 / self.limit as f64
    }

    pub fn day(&self, day: u32) -> Option<&DayCell> {
        self.cells.iter().find(|cell| cell.day == day)
    }

    pub fn spent_on(&self, day: u32) -> AmountType {
        self.day(day).map(|cell| cell.spent).unwrap_or(0)
    }

    /// 到某天为止的累计支出（含当天）。
    pub fn cumulative_through(&self, day: u32) -> AmountType {
        self.cells
            .iter()
            .filter(|cell| cell.day <= day)
            .map(|cell| cell.spent)
            .sum()
    }

    /// 到某天为止应该有的额度（含当天）。
    pub fn allowance_through(&self, day: u32) -> AmountType {
        self.daily_allowance * day as AmountType
    }

    /// 到某天为止的额度结余：正数是攒下的，负数是提前花了。
    pub fn allowance_balance_through(&self, day: u32) -> AmountType {
        self.allowance_through(day) - self.cumulative_through(day)
    }
}

/// 某月每天花了多少：只算**计入日历的计划**名下的支出。
///
/// `calendar` 就是 [`calendar_books`] 的结果 —— 账单跟着计划走，自己不存这个开关。
pub fn expense_by_day(
    month: NaiveDate,
    bills: &[BillEntity],
    calendar: &HashSet<BookId>,
) -> HashMap<u32, AmountType> {
    let mut by_day: HashMap<u32, AmountType> = HashMap::new();
    for bill in bills {
        if bill.kind != BillKind::Expense || !calendar.contains(&bill.book.id) {
            continue;
        }
        let date = bill.created_at.date_naive();
        if date.year() != month.year() || date.month() != month.month() {
            continue;
        }
        *by_day.entry(date.day()).or_insert(0) += bill.amount;
    }
    by_day
}

/// 某月每个收入计划进账多少：`book_id -> 收入合计`。
///
/// 收入不进日历，所以这里不按「是否计入日历」筛，也不看账单上的历史
/// `excluded` 标记：收入口径只认方向和日期。
pub fn income_by_plan(month: NaiveDate, bills: &[BillEntity]) -> HashMap<BookId, AmountType> {
    let mut by_plan: HashMap<BookId, AmountType> = HashMap::new();
    for bill in bills {
        if bill.kind != BillKind::Income {
            continue;
        }
        let date = bill.created_at.date_naive();
        if date.year() != month.year() || date.month() != month.month() {
            continue;
        }
        *by_plan.entry(bill.book.id).or_insert(0) += bill.amount;
    }
    by_plan
}

/// 某月每个开销计划花了多少：`book_id -> 支出合计`。
///
/// 计划自己那一行始终展示它名下的全部支出（哪怕它被标成不计入日历）——
/// 关掉的只是「进不进日历」，不是「记没记账」。
pub fn expense_by_plan(month: NaiveDate, bills: &[BillEntity]) -> HashMap<BookId, AmountType> {
    let mut by_plan: HashMap<BookId, AmountType> = HashMap::new();
    for bill in bills {
        if bill.kind != BillKind::Expense {
            continue;
        }
        let date = bill.created_at.date_naive();
        if date.year() != month.year() || date.month() != month.month() {
            continue;
        }
        *by_plan.entry(bill.book.id).or_insert(0) += bill.amount;
    }
    by_plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bill::model::{BillEntity, BillKind};
    use crate::bill_book::model::BillBook;
    use crate::category::model::Category;

    fn month() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 1).expect("合法月份")
    }

    fn plan(name: &str, limit: Option<AmountType>) -> Plan {
        plan_of(name, limit, PlanKind::Expense)
    }

    fn income_plan(name: &str, limit: Option<AmountType>) -> Plan {
        plan_of(name, limit, PlanKind::Income)
    }

    fn plan_of(name: &str, limit: Option<AmountType>, kind: PlanKind) -> Plan {
        Plan::new(
            BillBook {
                id: 1,
                name: name.to_string(),
            },
            limit,
            kind,
        )
    }

    fn bill(day: u32, amount: AmountType, book_id: BookId, excluded: bool) -> BillEntity {
        BillEntity {
            id: day as u64,
            kind: BillKind::Expense,
            amount,
            book: BillBook {
                id: book_id,
                name: format!("计划{book_id}"),
            },
            category: Category {
                id: 1,
                name: "餐饮".to_string(),
                parent_id: None,
            },
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 10, day)
                .expect("合法日期")
                .and_hms_opt(12, 0, 0)
                .expect("合法时间")
                .and_utc(),
            remark: None,
            excluded,
        }
    }

    #[test]
    fn capped_plan_knows_what_is_left() {
        let row = PlanRow::new(plan("房租", Some(3000_00)), 1200_00);
        assert_eq!(row.remaining(), Some(1800_00));
        assert!(!row.over());
        assert!((row.ratio() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn uncapped_plan_has_no_remaining_and_never_overflows() {
        let row = PlanRow::new(plan("零花", None), 9_000_00);
        assert_eq!(row.remaining(), None);
        assert!(!row.over());
        assert_eq!(row.ratio(), 0.0);
    }

    #[test]
    fn overspending_turns_negative() {
        let row = PlanRow::new(plan("房租", Some(1000_00)), 1200_00);
        assert_eq!(row.remaining(), Some(-200_00));
        assert!(row.over());
    }

    #[test]
    fn total_limit_only_counts_capped_expense_plans() {
        let rows = [
            PlanRow::new(plan("房租", Some(3000_00)), 0),
            // 收入计划的目标不占日历额度
            PlanRow::new(income_plan("工资", Some(8000_00)), 0),
            PlanRow::new(plan("零花", None), 500_00),
        ];
        assert_eq!(total_limit(&rows), 3000_00);
    }

    #[test]
    fn coverage_compares_allocated_to_income() {
        let coverage = PlanCoverage::new(10_000_00, 12_000_00, 0);
        assert!(!coverage.covers());
        assert_eq!(coverage.shortfall(), 2000_00);
        assert_eq!(coverage.remaining(), -2000_00);

        let exact = PlanCoverage::new(10_000_00, 10_000_00, 0);
        assert!(exact.covers());
        assert_eq!(exact.ratio(), 1.0);
    }

    #[test]
    fn coverage_deducts_uncapped_spending() {
        // 收入 10,000、额度 6,000、没上限的计划已经花了 1,500 → 还剩 2,500
        let coverage = PlanCoverage::new(10_000_00, 6_000_00, 1_500_00);
        assert_eq!(coverage.remaining(), 2_500_00);
        assert!(coverage.covers());

        // 扣掉之后不够分 → 算盖不住
        let short = PlanCoverage::new(10_000_00, 6_000_00, 5_000_00);
        assert_eq!(short.remaining(), -1_000_00);
        assert_eq!(short.shortfall(), 1_000_00);
        assert!(!short.covers());
    }

    #[test]
    fn uncapped_spent_sums_only_expense_plans_without_limit() {
        let rows = [
            // 没上限的开销计划：算
            PlanRow::new(plan("吃饭", None), 1_200_00),
            // 有上限的开销计划：额度已经覆盖，不重复扣
            PlanRow::new(plan("房租", Some(3_000_00)), 2_000_00),
            // 收入计划没有「已花」这回事
            PlanRow::new(income_plan("工资", Some(8_000_00)), 6_000_00),
            // 没目标也没进账，本来就是 0
            PlanRow::new(income_plan("外快", None), 300_00),
        ];
        assert_eq!(uncapped_spent(&rows), 1_200_00);
        assert_eq!(uncapped_spent(&[]), 0);
    }

    #[test]
    fn coverage_without_income_does_not_divide_by_zero() {
        let coverage = PlanCoverage::new(0, 5_000_00, 0);
        assert_eq!(coverage.ratio(), 0.0);
        assert!(!coverage.covers());
    }

    #[test]
    fn progress_without_limit_paints_nothing() {
        let mut spent = HashMap::new();
        spent.insert(3u32, 100_00);
        let progress = PlanProgress::build(month(), 0, Some(month().with_day(5).unwrap()), &spent);

        assert_eq!(progress.daily_allowance, 0);
        assert_eq!(progress.day(3).map(|cell| cell.status), Some(DayStatus::None));
        assert!(!progress.over_limit());
        assert_eq!(progress.used_ratio(), 0.0);
    }

    #[test]
    fn progress_marks_days_over_the_daily_allowance() {
        // 上限 3,100 分摊 31 天 = 日额度 100 分
        let mut spent = HashMap::new();
        spent.insert(2u32, 80_00);
        spent.insert(3u32, 150_00);
        let progress = PlanProgress::build(month(), 3_100_00, Some(month().with_day(4).unwrap()), &spent);

        assert_eq!(progress.daily_allowance, 100_00);
        assert_eq!(progress.day(2).map(|cell| cell.status), Some(DayStatus::WithinDaily));
        assert_eq!(progress.day(3).map(|cell| cell.status), Some(DayStatus::OverDaily));
        assert_eq!(progress.day(4).map(|cell| cell.status), Some(DayStatus::None));
        assert_eq!(progress.spent, 230_00);
    }

    #[test]
    fn progress_switches_to_over_limit_once_cumulative_passes_it() {
        let mut spent = HashMap::new();
        spent.insert(1u32, 60_00);
        spent.insert(2u32, 60_00);
        let progress = PlanProgress::build(month(), 100_00, Some(month().with_day(2).unwrap()), &spent);

        assert_eq!(progress.day(2).map(|cell| cell.status), Some(DayStatus::OverLimit));
        assert!(progress.over_limit());
        assert_eq!(progress.remaining(), -20_00);
    }

    #[test]
    fn allowance_balance_tracks_savings_through_the_month() {
        let mut spent = HashMap::new();
        spent.insert(1u32, 10_00);
        spent.insert(2u32, 90_00);
        let progress = PlanProgress::build(month(), 3_100_00, None, &spent);

        assert_eq!(progress.allowance_through(2), 200_00);
        assert_eq!(progress.cumulative_through(2), 100_00);
        assert_eq!(progress.allowance_balance_through(2), 100_00);
    }

    #[test]
    fn expense_by_day_skips_plans_out_of_calendar_and_other_months() {
        let bills = vec![
            bill(1, 30_00, 1, false),
            bill(1, 20_00, 1, false),
            // 计划 2 不计入日历：它名下的支出不该进日历
            bill(2, 99_00, 2, false),
            bill(2, 10_00, 1, false),
        ];
        let calendar: HashSet<BookId> = [1].into_iter().collect();

        let by_day = expense_by_day(month(), &bills, &calendar);
        assert_eq!(by_day.get(&1).copied(), Some(50_00));
        assert_eq!(by_day.get(&2).copied(), Some(10_00));

        let other = NaiveDate::from_ymd_opt(2026, 9, 1).expect("合法月份");
        assert!(expense_by_day(other, &bills, &calendar).is_empty());
    }

    #[test]
    fn expense_by_plan_counts_every_bill_of_the_plan() {
        // 计划自己那一行始终展示名下全部支出：关掉的只是「进不进日历」
        let bills = vec![
            bill(1, 30_00, 1, false),
            bill(2, 70_00, 2, false),
            bill(3, 99_00, 2, false),
        ];
        let by_plan = expense_by_plan(month(), &bills);
        assert_eq!(by_plan.get(&1).copied(), Some(30_00));
        assert_eq!(by_plan.get(&2).copied(), Some(169_00));
    }

    #[test]
    fn calendar_books_only_takes_plans_in_calendar() {
        let rows = [
            PlanRow::new(plan("房租", Some(2500_00)), 0),
            PlanRow::new(plan("报销", Some(1000_00)).with_calendar(false), 0),
            PlanRow::new(income_plan("工资", Some(8000_00)), 0),
        ];
        let books = calendar_books(&rows);
        assert!(books.contains(&1), "计入日历的计划要在集合里");
        assert_eq!(books.len(), 1, "不计入日历的计划不进集合");
    }

    #[test]
    fn coverage_still_counts_plans_out_of_calendar() {
        // 覆盖校验看的是「钱怎么分」，跟这笔支出进不进日历无关：
        // 不计入日历的计划，额度也要算进开销额度，没上限的已花也要从还剩里扣。
        let rows = [
            PlanRow::new(plan("房租", Some(2_500_00)), 1_000_00),
            PlanRow::new(plan("报销", Some(1_000_00)).with_calendar(false), 800_00),
            PlanRow::new(plan("吃饭", None).with_calendar(false), 300_00),
        ];
        assert_eq!(total_limit(&rows), 3_500_00, "覆盖校验：不计入日历的额度也要算");
        assert_eq!(calendar_limit(&rows), 2_500_00, "日历额度：只算计入日历的计划");
        assert_eq!(uncapped_spent(&rows), 300_00, "不计入日历的已花也要扣");
        // 但日历的支出口径仍旧只认计入日历的计划
        let calendar = calendar_books(&rows);
        assert_eq!(calendar.len(), 1, "日历只筛出房租");
    }

    #[test]
    fn total_income_sums_only_income_plan_targets() {
        let rows = [
            PlanRow::new(income_plan("工资", Some(8000_00)), 0),
            // 没设目标的收入计划不参与：它没有承诺可对照
            PlanRow::new(income_plan("外快", None), 0),
            // 开销计划的额度不能算进收入
            PlanRow::new(plan("房租", Some(2500_00)), 0),
        ];
        assert_eq!(total_income(&rows), 8000_00);
        assert_eq!(total_income(&[]), 0);
    }

    #[test]
    fn income_by_plan_sums_income_bills_even_when_excluded() {
        let mut salary = bill(1, 800_00, 2, false);
        salary.kind = BillKind::Income;
        // 收入口径不看「不计入」：走账的奖金也照样算进这个月的收入
        let mut bonus = bill(2, 200_00, 2, true);
        bonus.kind = BillKind::Income;
        let bills = vec![salary, bonus, bill(3, 99_00, 2, false)];

        let by_plan = income_by_plan(month(), &bills);
        assert_eq!(by_plan.get(&2).copied(), Some(1000_00));
        assert_eq!(by_plan.get(&1).copied(), None);

        let other = NaiveDate::from_ymd_opt(2026, 9, 1).expect("合法月份");
        assert!(income_by_plan(other, &bills).is_empty());
    }

    #[test]
    fn describe_limit_speaks_the_language_of_each_kind() {
        assert_eq!(plan("房租", Some(3000_00)).describe_limit(), "每月 3,000.00 元");
        assert_eq!(
            plan("零花", None).describe_limit(),
            "不设上限，只从 0 开始累计"
        );
        assert_eq!(
            income_plan("工资", Some(8000_00)).describe_limit(),
            "每月目标 8,000.00 元"
        );
        assert_eq!(
            income_plan("外快", None).describe_limit(),
            "不设目标，只累计入账"
        );
    }

    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap()), 29);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()), 28);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()), 31);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()), 31);
    }
}
