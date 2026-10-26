//! 计划的读模型：一个计划 = 一个账本 + 可选上限。
//!
//! 计划本体存在 `bill_books`（见 [`crate::bill_book::model::Plan`]），这里只放
//! 「把这个月的账单摊到计划上」用的派生结构：谁花了多少、还剩多少，
//! 以及支出日历要的日额度与逐日状态。
//!
//! 两种计划共用一个列表：
//!
//! * 设了上限（`plan.limit = Some(n)`）：有额度、能算剩余、能上色日历；
//! * 没设上限（`None`）：从 0 开始记流水，只看已花。

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate};

use crate::bill::model::{BillEntity, BillKind};
use crate::bill_book::model::Plan;
use crate::common::types::{AmountType, BookId};

/// 一个计划在本月的执行情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub plan: Plan,
    /// 本月记在这个计划名下的支出合计（「不计入」的账单不算）。
    pub spent: AmountType,
}

impl PlanRow {
    pub fn new(plan: Plan, spent: AmountType) -> Self {
        Self { plan, spent }
    }

    pub fn name(&self) -> &str {
        self.plan.name()
    }

    /// 还剩多少：只有设了上限才有意义，没上限返回 `None`（不拿它当 0 用）。
    pub fn remaining(&self) -> Option<AmountType> {
        self.plan.limit.map(|limit| limit - self.spent)
    }

    /// 超支了吗：没上限的计划永远不会超。
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

/// 所有计划的上限合计：日历与概览的「本月额度」都取这个数。
///
/// 没上限的计划不参与——它们没有额度可摊，摊进去会把日额度算虚。
pub fn total_limit(rows: &[PlanRow]) -> AmountType {
    rows.iter()
        .filter_map(|row| row.plan.limit)
        .filter(|limit| *limit > 0)
        .sum()
}

/// 上限的一句话说明：有上限说额度，没上限说它是累计型的。
///
/// 计划列表、删除确认、表单提示都要讲这件事，措辞只在这里维护一份。
pub fn describe_limit(limit: Option<AmountType>) -> String {
    match limit {
        Some(limit) if limit > 0 => format!("每月 {} 元", crate::common::money::format_amount(limit)),
        _ => "不设上限，只从 0 开始累计".to_string(),
    }
}

/// 计划合计 vs 当月收入：这张卡回答「钱分完了吗、分超了吗」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCoverage {
    /// 当月收入（工资）。
    pub income: AmountType,
    /// 有上限计划的合计。
    pub allocated: AmountType,
}

impl PlanCoverage {
    pub fn new(income: AmountType, allocated: AmountType) -> Self {
        Self { income, allocated }
    }

    /// 收入减计划：正数=还有没分配的钱，负数=分超了。
    pub fn remaining(&self) -> AmountType {
        self.income - self.allocated
    }

    /// 分超的部分（没超就是 0）。
    pub fn shortfall(&self) -> AmountType {
        (-self.remaining()).max(0)
    }

    /// 能不能覆盖。等号算覆盖——刚好分完也是一种答案。
    pub fn covers(&self) -> bool {
        self.remaining() >= 0
    }

    /// 计划占收入的比例；收入为 0 时给 0，避免除零。
    pub fn ratio(&self) -> f64 {
        if self.income <= 0 {
            return 0.0;
        }
        self.allocated as f64 / self.income as f64
    }
}

/// 当月收入合计。`excluded` 只影响支出日历与计划执行，不影响收入口径。
pub fn month_income(month: NaiveDate, bills: &[BillEntity]) -> AmountType {
    bills
        .iter()
        .filter(|bill| bill.kind == BillKind::Income)
        .filter(|bill| {
            let date = bill.created_at.date_naive();
            date.year() == month.year() && date.month() == month.month()
        })
        .map(|bill| bill.amount)
        .sum()
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
    /// 本月上限 = 所有有上限计划之和。
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

/// 某月每天花了多少（不含「不计入日历」的支出）。
pub fn expense_by_day(month: NaiveDate, bills: &[BillEntity]) -> HashMap<u32, AmountType> {
    let mut by_day: HashMap<u32, AmountType> = HashMap::new();
    for bill in bills {
        if bill.kind != BillKind::Expense || bill.excluded {
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

/// 某月每个计划花了多少：`book_id -> 支出合计`（不含「不计入日历」的支出）。
pub fn expense_by_plan(month: NaiveDate, bills: &[BillEntity]) -> HashMap<BookId, AmountType> {
    let mut by_plan: HashMap<BookId, AmountType> = HashMap::new();
    for bill in bills {
        if bill.kind != BillKind::Expense || bill.excluded {
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
        Plan::new(
            BillBook {
                id: 1,
                name: name.to_string(),
            },
            limit,
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
    fn total_limit_only_counts_capped_plans() {
        let rows = [
            PlanRow::new(plan("房租", Some(3000_00)), 0),
            PlanRow::new(plan("零花", None), 500_00),
        ];
        assert_eq!(total_limit(&rows), 3000_00);
    }

    #[test]
    fn coverage_compares_allocated_to_income() {
        let coverage = PlanCoverage::new(10_000_00, 12_000_00);
        assert!(!coverage.covers());
        assert_eq!(coverage.shortfall(), 2000_00);
        assert_eq!(coverage.remaining(), -2000_00);

        let exact = PlanCoverage::new(10_000_00, 10_000_00);
        assert!(exact.covers());
        assert_eq!(exact.ratio(), 1.0);
    }

    #[test]
    fn coverage_without_income_does_not_divide_by_zero() {
        let coverage = PlanCoverage::new(0, 5_000_00);
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
    fn expense_by_day_skips_excluded_and_other_months() {
        let bills = vec![
            bill(1, 30_00, 1, false),
            bill(1, 20_00, 1, false),
            bill(1, 99_00, 1, true),
            bill(2, 10_00, 1, false),
        ];
        let by_day = expense_by_day(month(), &bills);
        assert_eq!(by_day.get(&1).copied(), Some(50_00));
        assert_eq!(by_day.get(&2).copied(), Some(10_00));

        let other = NaiveDate::from_ymd_opt(2026, 9, 1).expect("合法月份");
        assert!(expense_by_day(other, &bills).is_empty());
    }

    #[test]
    fn expense_by_plan_splits_by_book_and_skips_excluded() {
        let bills = vec![
            bill(1, 30_00, 1, false),
            bill(2, 70_00, 2, false),
            bill(3, 99_00, 2, true),
        ];
        let by_plan = expense_by_plan(month(), &bills);
        assert_eq!(by_plan.get(&1).copied(), Some(30_00));
        assert_eq!(by_plan.get(&2).copied(), Some(70_00));
    }

    #[test]
    fn month_income_sums_only_income_bills() {
        let mut income = bill(5, 500_00, 1, false);
        income.kind = BillKind::Income;
        let bills = vec![income, bill(6, 100_00, 1, false)];
        assert_eq!(month_income(month(), &bills), 500_00);
    }

    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap()), 29);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()), 28);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()), 31);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()), 31);
    }
}
