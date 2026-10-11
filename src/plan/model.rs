//! 开销计划的数据结构与规则。
//!
//! 一个月的计划只有两个数：**预计金额**和**攒钱金额**，上限开销由两者相减推出——
//! 存三个数迟早会互相打架（改了一个忘了改另一个）。
//!
//! 日历配色规则见 [`DayStatus`]：
//! - 灰：这天没有支出（含还没到的日期）；
//! - 绿：有支出且没超**日额度**（上限开销 ÷ 当月天数）；
//! - 黄：当天支出超了日额度；
//! - 红：**到这天为止累计**已经超过上限开销——也就是预算崩掉的那天起开始红。

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate};

use crate::bill::model::{BillEntity, BillKind};
use crate::common::types::AmountType;

/// 超支判断模式：决定日历上「红」是怎么来的。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlanMode {
    /// **累计**（默认）：到当天为止累计超过上限，则当天起一路标红。
    #[default]
    Cumulative,
    /// **只看当天**：每天独立判断，某天爆掉不连累后面的日子。
    DailyOnly,
}

impl PlanMode {
    /// 落库用的字面量。
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanMode::Cumulative => "cumulative",
            PlanMode::DailyOnly => "daily",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cumulative" | "累计" => Some(PlanMode::Cumulative),
            "daily" | "daily_only" | "按天" | "每日" => Some(PlanMode::DailyOnly),
            _ => None,
        }
    }

    /// 界面上的名字（也是设置表单里的选项文本）。
    pub fn label(&self) -> &'static str {
        match self {
            PlanMode::Cumulative => "累计标红",
            PlanMode::DailyOnly => "只看当天",
        }
    }
}

/// 一个月的资金计划。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendingPlan {
    /// 这个月预计到手多少（分）。
    pub expected: AmountType,
    /// 打算攒下多少（分）。
    pub savings: AmountType,
    /// 超支判断模式。
    pub mode: PlanMode,
}

impl SpendingPlan {
    pub fn new(expected: AmountType, savings: AmountType) -> Self {
        Self {
            expected,
            savings,
            mode: PlanMode::default(),
        }
    }

    /// 换个超支判断模式。
    pub fn with_mode(mut self, mode: PlanMode) -> Self {
        self.mode = mode;
        self
    }

    /// 是否设置过（全 0 视为没设）。
    pub fn is_set(&self) -> bool {
        self.expected != 0 || self.savings != 0
    }

    /// 上限开销 = 预计 − 攒钱。攒钱比预计还多时按 0 算，避免出现负预算。
    pub fn limit(&self) -> AmountType {
        (self.expected - self.savings).max(0)
    }

    /// 日额度 = 上限开销 ÷ 当月天数（向下取整到分）。
    pub fn daily_allowance(&self, month: NaiveDate) -> AmountType {
        let days = days_in_month(month) as i64;
        if days <= 0 {
            0
        } else {
            self.limit() / days
        }
    }
}

/// 当月天数。
pub fn days_in_month(month: NaiveDate) -> u32 {
    let first = NaiveDate::from_ymd_opt(month.year(), month.month(), 1).unwrap_or(month);
    let next = if month.month() == 12 {
        NaiveDate::from_ymd_opt(month.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(month.year(), month.month() + 1, 1)
    };

    match next {
        Some(next) => (next - first).num_days() as u32,
        None => 30,
    }
}

/// 日历上一格的颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayStatus {
    /// 这天没有支出。
    None,
    /// 有支出，且在日额度内。
    WithinDaily,
    /// 当天支出超了日额度。
    OverDaily,
    /// 到这天为止累计已超上限开销。
    OverLimit,
}

/// 日历上的一格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayCell {
    pub day: u32,
    pub spent: AmountType,
    pub status: DayStatus,
}

/// 当月计划的执行情况。
#[derive(Debug, Clone)]
pub struct PlanProgress {
    pub month: NaiveDate,
    pub plan: SpendingPlan,
    /// 上限开销（= 预计 − 攒钱）。
    pub limit: AmountType,
    /// 日额度（= 上限开销 ÷ 当月天数）。
    pub daily_allowance: AmountType,
    /// 本月已用（只算纳入统计的支出）。
    pub spent: AmountType,
    /// 日均：已用 ÷ 已过天数。
    pub daily_average: AmountType,
    /// 当月每一天的格子，下标 = 天 − 1。
    pub cells: Vec<DayCell>,
}

impl PlanProgress {
    /// 按当月支出构造。
    ///
    /// `spending_by_day` 是「第几天 → 当天支出」，只应包含**纳入统计的支出**；
    /// `today` 传 `None` 表示看的是历史月份，日均按整月天数算。
    pub fn build(
        month: NaiveDate,
        plan: SpendingPlan,
        today: Option<NaiveDate>,
        spending_by_day: &HashMap<u32, AmountType>,
    ) -> Self {
        let days = days_in_month(month);
        let limit = plan.limit();
        let daily_allowance = plan.daily_allowance(month);

        let mut cells = Vec::with_capacity(days as usize);
        let mut cumulative: AmountType = 0;

        for day in 1..=days {
            let spent = spending_by_day.get(&day).copied().unwrap_or(0);
            cumulative += spent;

            // 「红」的判定随模式而变：累计看的是到今天的总和，按天只看当天那一笔
            let over_limit = limit > 0
                && match plan.mode {
                    PlanMode::Cumulative => cumulative > limit,
                    PlanMode::DailyOnly => spent > limit,
                };

            let status = if spent <= 0 {
                DayStatus::None
            } else if over_limit {
                DayStatus::OverLimit
            } else if daily_allowance > 0 && spent > daily_allowance {
                DayStatus::OverDaily
            } else {
                DayStatus::WithinDaily
            };

            cells.push(DayCell { day, spent, status });
        }

        let spent: AmountType = cells.iter().map(|cell| cell.spent).sum();

        // 日均按「已经过去的天数」算，否则月初看会低得离谱
        let elapsed = match today {
            Some(today) if today.year() == month.year() && today.month() == month.month() => {
                (today.day() as i64).max(1)
            }
            _ => days as i64,
        };
        let daily_average = spent / elapsed.max(1);

        Self {
            month,
            plan,
            limit,
            daily_allowance,
            spent,
            daily_average,
            cells,
        }
    }

    /// 是否已经超出上限开销。
    pub fn over_limit(&self) -> bool {
        self.limit > 0 && self.spent > self.limit
    }

    /// 还剩多少可用（超了就是 0）。
    pub fn remaining(&self) -> AmountType {
        (self.limit - self.spent).max(0)
    }

    /// 用掉的比例（0.0–1.0）；没设计划时为 0。
    pub fn used_ratio(&self) -> f64 {
        if self.limit <= 0 {
            0.0
        } else {
            (self.spent as f64 / self.limit as f64).clamp(0.0, 1.0)
        }
    }

    /// 取某一天的格子。
    pub fn day(&self, day: u32) -> Option<&DayCell> {
        if day == 0 {
            return None;
        }
        self.cells.get((day - 1) as usize)
    }

    /// 某一天花了多少（越界按 0 算）。
    pub fn spent_on(&self, day: u32) -> AmountType {
        self.day(day).map(|cell| cell.spent).unwrap_or(0)
    }

    /// 从月初到第 `day` 天为止的累计支出。
    pub fn cumulative_through(&self, day: u32) -> AmountType {
        self.cells
            .iter()
            .take(day as usize)
            .map(|cell| cell.spent)
            .sum()
    }

    /// 从月初到第 `day` 天为止**应该花掉**的额度（日额度 × 天数）。
    pub fn allowance_through(&self, day: u32) -> AmountType {
        self.daily_allowance * day as AmountType
    }

    /// 到第 `day` 天为止的**额度余额** = 日额度 × 天数 − 累计开销。
    ///
    /// 这就是「上一日余额 + 当日额度 − 当日开销」逐日滚下来的结果
    /// （也等于「上一日余额 + 当日结余」）；花超前了就是负数。
    pub fn allowance_balance_through(&self, day: u32) -> AmountType {
        self.allowance_through(day) - self.cumulative_through(day)
    }
}

/// 把当月账单按「第几天」汇总**纳入统计的支出**。
///
/// 收入不算开销；`excluded` 的账单（报销、代付、走账）整条跳过；
/// 别的月份直接忽略。
pub fn expense_by_day(month: NaiveDate, bills: &[BillEntity]) -> HashMap<u32, AmountType> {
    let mut totals: HashMap<u32, AmountType> = HashMap::new();

    for bill in bills {
        if bill.excluded || bill.kind != BillKind::Expense {
            continue;
        }

        let date = bill.created_at.date_naive();
        if date.year() != month.year() || date.month() != month.month() {
            continue;
        }

        *totals.entry(date.day()).or_default() += bill.amount;
    }

    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{TimeZone, Utc};

    use crate::bill_book::model::BillBook;
    use crate::category::model::Category;

    fn month() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
    }

    fn bill(
        id: u64,
        kind: BillKind,
        amount: AmountType,
        month: u32,
        day: u32,
        excluded: bool,
    ) -> BillEntity {
        BillEntity {
            id,
            kind,
            amount,
            book: BillBook {
                id: 1,
                name: "微信".into(),
            },
            category: Category {
                id: 1,
                parent_id: None,
                name: "餐饮".into(),
            },
            created_at: Utc.with_ymd_and_hms(2026, month, day, 12, 0, 0).unwrap(),
            remark: None,
            excluded,
        }
    }

    #[test]
    fn limit_is_derived_from_expected_minus_savings() {
        let plan = SpendingPlan::new(12_000_00, 8_000_00);
        assert_eq!(plan.limit(), 4_000_00);
        assert!(plan.is_set());

        // 攒钱比预计还多 → 上限按 0 算，不出负数
        assert_eq!(SpendingPlan::new(100, 500).limit(), 0);

        assert!(!SpendingPlan::default().is_set());
    }

    #[test]
    fn daily_allowance_splits_by_month_length() {
        assert_eq!(days_in_month(month()), 31);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()), 28);
        assert_eq!(days_in_month(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap()), 29);

        let plan = SpendingPlan::new(3_100_00, 0);
        assert_eq!(plan.daily_allowance(month()), 100_00);
    }

    #[test]
    fn paints_days_green_yellow_red() {
        // 上限 4000.00，10 月 31 天 → 日额度 129.03
        let plan = SpendingPlan::new(12_000_00, 8_000_00);

        let mut by_day = HashMap::new();
        by_day.insert(1, 100_00); // 100 ≤ 129.03 → 绿
        by_day.insert(2, 200_00); // 200 > 129.03 → 黄
        // 3 号没支出 → 灰
        by_day.insert(4, 3_800_00); // 累计 4100 > 4000 → 红

        let progress = PlanProgress::build(month(), plan, None, &by_day);

        assert_eq!(progress.day(1).unwrap().status, DayStatus::WithinDaily);
        assert_eq!(progress.day(2).unwrap().status, DayStatus::OverDaily);
        assert_eq!(progress.day(3).unwrap().status, DayStatus::None);
        assert_eq!(progress.day(4).unwrap().status, DayStatus::OverLimit);
        assert_eq!(progress.day(5).unwrap().status, DayStatus::None);

        assert_eq!(progress.spent, 100_00 + 200_00 + 3_800_00);
        assert!(progress.over_limit());
        assert_eq!(progress.remaining(), 0);
    }

    #[test]
    fn without_a_plan_nothing_turns_red() {
        // 没设计划就不做超上限判断，否则整月都会变红
        let mut by_day = HashMap::new();
        by_day.insert(1, 5_000_00);

        let progress = PlanProgress::build(month(), SpendingPlan::default(), None, &by_day);
        assert_eq!(progress.day(1).unwrap().status, DayStatus::WithinDaily);
        assert!(!progress.over_limit());
        assert_eq!(progress.used_ratio(), 0.0);
    }

    #[test]
    fn mode_round_trips_its_db_literal() {
        for mode in [PlanMode::Cumulative, PlanMode::DailyOnly] {
            assert_eq!(PlanMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(PlanMode::parse("按天"), Some(PlanMode::DailyOnly));
        assert_eq!(PlanMode::parse("unknown"), None);
        assert_eq!(SpendingPlan::new(1, 0).mode, PlanMode::Cumulative);
    }

    #[test]
    fn cumulative_mode_paints_everything_after_the_break_red() {
        // 上限 100.00，日额度 100/31 ≈ 3.22
        let plan = SpendingPlan::new(200_00, 100_00);
        let mut by_day = HashMap::new();
        by_day.insert(1, 60_00);
        by_day.insert(2, 60_00); // 累计 120 > 100 → 从这天起红
        by_day.insert(3, 1_00);  // 明明没花多少，也还是红

        let progress = PlanProgress::build(month(), plan, None, &by_day);
        assert_eq!(progress.day(1).unwrap().status, DayStatus::OverDaily);
        assert_eq!(progress.day(2).unwrap().status, DayStatus::OverLimit);
        assert_eq!(progress.day(3).unwrap().status, DayStatus::OverLimit);
    }

    #[test]
    fn daily_only_mode_does_not_punish_later_days() {
        // 同一条数据，换成「只看当天」：3 号不该跟着红
        let plan = SpendingPlan::new(200_00, 100_00).with_mode(PlanMode::DailyOnly);
        let mut by_day = HashMap::new();
        by_day.insert(1, 60_00);
        by_day.insert(2, 60_00);
        by_day.insert(3, 1_00);

        let progress = PlanProgress::build(month(), plan, None, &by_day);
        assert_eq!(progress.day(1).unwrap().status, DayStatus::OverDaily);
        assert_eq!(progress.day(2).unwrap().status, DayStatus::OverDaily);
        assert_eq!(
            progress.day(3).unwrap().status,
            DayStatus::WithinDaily,
            "某天爆掉不该连累后面的日子"
        );
    }

    #[test]
    fn daily_only_mode_still_flags_a_day_that_eats_the_whole_limit() {
        let plan = SpendingPlan::new(200_00, 100_00).with_mode(PlanMode::DailyOnly);
        let mut by_day = HashMap::new();
        by_day.insert(1, 120_00); // 单日就超过上限 100.00

        let progress = PlanProgress::build(month(), plan, None, &by_day);
        assert_eq!(progress.day(1).unwrap().status, DayStatus::OverLimit);
    }

    #[test]
    fn allowance_balance_rolls_over_day_by_day() {
        // 上限 3100.00，10 月 31 天 → 日额度 100.00
        let plan = SpendingPlan::new(3_100_00, 0);

        let mut by_day = HashMap::new();
        by_day.insert(1, 30_00); // 100 - 30 = 70
        by_day.insert(2, 150_00); // 70 + 100 - 150 = 20
        // 3 号没花钱：20 + 100 = 120

        let progress = PlanProgress::build(month(), plan, None, &by_day);

        assert_eq!(progress.allowance_balance_through(1), 70_00);
        assert_eq!(progress.allowance_balance_through(2), 20_00);
        assert_eq!(
            progress.allowance_balance_through(3),
            120_00,
            "没花钱的日子额度会攒下来"
        );

        // 就是「上一日余额 + 当日结余」
        assert_eq!(
            progress.allowance_balance_through(2),
            progress.allowance_balance_through(1) + progress.daily_allowance - 150_00
        );
    }

    #[test]
    fn allowance_balance_goes_negative_when_overspent() {
        let plan = SpendingPlan::new(3_100_00, 0);
        let mut by_day = HashMap::new();
        by_day.insert(1, 250_00); // 100 - 250 = -150

        let progress = PlanProgress::build(month(), plan, None, &by_day);
        assert_eq!(progress.allowance_balance_through(1), -150_00);
    }

    #[test]
    fn daily_average_divides_by_elapsed_days() {
        let plan = SpendingPlan::new(3_100_00, 0);
        let mut by_day = HashMap::new();
        by_day.insert(1, 100_00);
        by_day.insert(2, 100_00);

        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let progress = PlanProgress::build(month(), plan, Some(today), &by_day);
        assert_eq!(progress.daily_average, 20_00, "看本月第 10 天 → 分母 10");

        // 看历史月份 → 分母取整月天数
        let history = PlanProgress::build(month(), plan, None, &by_day);
        assert_eq!(history.daily_average, 200_00 / 31);
    }

    #[test]
    fn excluded_bills_stay_out_of_the_calendar() {
        let bills = vec![
            bill(1, BillKind::Expense, 100_00, 10, 5, false),
            bill(2, BillKind::Expense, 900_00, 10, 5, true), // 不计入统计
            bill(3, BillKind::Income, 500_00, 10, 5, false), // 收入不算开销
            bill(4, BillKind::Expense, 50_00, 9, 5, false),  // 别的月份
        ];

        let by_day = expense_by_day(month(), &bills);
        assert_eq!(by_day.get(&5), Some(&100_00));
        assert_eq!(by_day.len(), 1, "只应留下一笔");
    }
}
