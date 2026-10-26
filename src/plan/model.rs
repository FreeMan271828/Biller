//! 资金计划的数据模型：把这个月的钱拆成若干块，再看合计能不能被收入覆盖。
//!
//! 两条边界要分清：
//!
//! * 计划是**全局**的——「日常开销 3000 / 存款 9000 / VPN 50」这个分配每个月都适用，
//!   换个月份不该重填一遍；
//! * 收入是**按月**的——「合计能不能覆盖收入」只有拿到某个月的收入之后才有答案，
//!   所以判定放在 [`PlanCoverage`] 里，由 service 用当月账单算出来。
//!
//! 日历上的红黄绿也和计划有关，但那属于「花的钱 vs 计划的总额度」，
//! 与分块本身无关，因此仍由 [`PlanProgress`] 统一计算。

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate};

use crate::bill::model::{BillEntity, BillKind};
use crate::category::model::{Category, parent_names};
use crate::common::types::AmountType;

/// 计划里的一块：名字 + 每月金额（分），可选绑定一个支出分类。
///
/// 绑了分类的块会拿「该分类（含子分类）本月实际花的钱」去对账，
/// 于是计划、分类、账单三者咬合成一条链，不再各维护一套名字：
///
/// * **账本** = 钱从哪儿进出（微信 / 支付宝 / 银行卡），只是账户标签；
/// * **分类** = 这笔钱花在什么事上（餐饮 / 交通 / 房租），账单打标签；
/// * **计划** = 这个月打算在各类事上各花多少，绑定分类后自动出「已花 / 剩余」。
///
/// 没绑分类的块（比如「存款」「VPN 年费」）就是自由预算：
/// 只占额度、不跟账单对账，因此已花是 `None` 而不是 0——
/// 「没对账」和「花了 0 元」是两件事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub name: String,
    pub amount: AmountType,
    /// 绑定的分类名（顶层分类名即可；父分类自动包含子分类的支出）。
    pub category: Option<String>,
}

impl PlanItem {
    pub fn new(name: impl Into<String>, amount: AmountType) -> Self {
        Self {
            name: name.into(),
            amount,
            category: None,
        }
    }

    /// 绑定分类。空白名归一成 `None`（= 不绑定）。
    pub fn with_category(mut self, category: Option<&str>) -> Self {
        self.category = normalize_category(category);
        self
    }

    /// 有没有对账对象。
    pub fn is_bound(&self) -> bool {
        self.category.is_some()
    }
}

/// 分类名的归一化：去空白、空串当没绑。
fn normalize_category(category: Option<&str>) -> Option<String> {
    category
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// 一整套资金分配计划。顺序即展示顺序，由用户自己维护。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpendingPlan {
    items: Vec<PlanItem>,
}

impl SpendingPlan {
    /// 最多几块。够细，又不至于让面板滚成一屏。
    pub const MAX_ITEMS: usize = 24;

    pub fn new(items: Vec<PlanItem>) -> Self {
        Self { items }
    }

    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }

    pub fn into_items(self) -> Vec<PlanItem> {
        self.items
    }

    pub fn is_set(&self) -> bool {
        !self.items.is_empty()
    }

    /// 合计 = 所有块加起来。月上限就是它。
    pub fn total(&self) -> AmountType {
        self.items.iter().map(|item| item.amount).sum()
    }

    /// 兼容旧名字：日历 / 进度仍然按「本月上限」算。
    pub fn limit(&self) -> AmountType {
        self.total()
    }

    pub fn get(&self, index: usize) -> Option<&PlanItem> {
        self.items.get(index)
    }

    /// 按名字找位置；CLI 的 add/remove 与表单查重都靠它。
    pub fn position(&self, name: &str) -> Option<usize> {
        self.items
            .iter()
            .position(|item| item.name.trim() == name.trim())
    }

    /// 同名覆盖、否则追加。CLI 的 `plan set` 靠它保持幂等。
    ///
    /// `category` 是「这一块绑哪个分类」，`None` 表示不绑定；
    /// 「没传就沿用旧值」的判断在 service 层，模型这里只做赋值。
    pub fn upsert(&mut self, name: impl Into<String>, amount: AmountType, category: Option<&str>) {
        let name = name.into();
        let category = normalize_category(category);
        if let Some(index) = self.position(&name) {
            self.items[index].name = name;
            self.items[index].amount = amount;
            self.items[index].category = category;
        } else {
            let mut item = PlanItem::new(name, amount);
            item.category = category;
            self.items.push(item);
        }
    }

    /// 替换第 `index` 块（表单编辑用）。分类一并替换，传 `None` 就是解绑。
    pub fn replace(
        &mut self,
        index: usize,
        name: impl Into<String>,
        amount: AmountType,
        category: Option<&str>,
    ) {
        if let Some(item) = self.items.get_mut(index) {
            item.name = name.into();
            item.amount = amount;
            item.category = normalize_category(category);
        }
    }

    pub fn remove(&mut self, index: usize) -> Option<PlanItem> {
        self.items.get(index)?;
        Some(self.items.remove(index))
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// 保存前的校验：名字要能读懂、金额不能为负、不能重名。
    ///
    /// 这里只管「计划本身自洽」，管不到「合计是否超过收入」——
    /// 超了要提醒，但不是错误：收入本来就可能晚到账。
    pub fn validate(&self) -> Result<(), String> {
        if self.items.len() > Self::MAX_ITEMS {
            return Err(format!(
                "计划最多 {} 块，当前 {} 块",
                Self::MAX_ITEMS,
                self.items.len()
            ));
        }

        let mut seen: Vec<&str> = Vec::with_capacity(self.items.len());
        let mut bound: Vec<&str> = Vec::new();
        for item in &self.items {
            let name = item.name.trim();
            if name.is_empty() {
                return Err("每一块都要有名字".to_string());
            }
            if name.chars().any(|character| character.is_control()) {
                return Err(format!("「{name}」里有不可见字符，换个名字"));
            }
            if item.amount < 0 {
                return Err(format!("「{name}」的金额不能为负"));
            }
            if seen.iter().any(|other| *other == name) {
                return Err(format!("「{name}」重复了，同一块只留一行"));
            }
            seen.push(name);

            // 一个分类只能有一份预算，否则「已花」会被算两遍，
            // 计划里也会出现两个抢同一笔钱的块。
            if let Some(category) = item.category.as_deref().map(str::trim) {
                if category.is_empty() {
                    continue;
                }
                if bound.iter().any(|other| *other == category) {
                    return Err(format!("分类「{category}」已经绑给别的块了，一个分类只留一份预算"));
                }
                bound.push(category);
            }
        }

        Ok(())
    }
}

/// 计划合计 vs 当月收入：这张卡回答「钱分完了吗、分超了吗」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCoverage {
    /// 当月收入（工资）。
    pub income: AmountType,
    /// 计划合计。
    pub allocated: AmountType,
}

impl PlanCoverage {
    pub fn new(income: AmountType, plan: &SpendingPlan) -> Self {
        Self {
            income,
            allocated: plan.total(),
        }
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
    /// 当天没有任何支出（或还没设计划）。
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

/// 当月执行情况：把 [`SpendingPlan`] 的合计摊到每一天，再和账单对账。
#[derive(Debug, Clone)]
pub struct PlanProgress {
    pub month: NaiveDate,
    /// 本月上限 = 计划合计。
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
        plan: &SpendingPlan,
        today: Option<NaiveDate>,
        spending_by_day: &HashMap<u32, AmountType>,
    ) -> Self {
        let days = days_in_month(month).max(1);
        let limit = plan.limit();
        let daily_allowance = if limit > 0 { limit / days as i64 } else { 0 };

        let mut cells = Vec::with_capacity(days as usize);
        let mut spent = 0;
        let mut cumulative = 0;
        for day in 1..=days {
            let day_spent = spending_by_day.get(&day).copied().unwrap_or(0);
            spent += day_spent;
            cumulative += day_spent;

            let status = if limit <= 0 {
                DayStatus::None
            } else if cumulative > limit {
                DayStatus::OverLimit
            } else if day_spent > daily_allowance {
                DayStatus::OverDaily
            } else if day_spent > 0 {
                DayStatus::WithinDaily
            } else {
                DayStatus::None
            };

            cells.push(DayCell {
                day,
                spent: day_spent,
                status,
            });
        }

        // 日均的分母取「已经过去的天数」：月初看日均时不该被未来 20 天摊薄
        let elapsed = match today {
            Some(today) if today.year() == month.year() && today.month() == month.month() => {
                today.day().clamp(1, days)
            }
            _ => days,
        };
        let daily_average = spent / elapsed as i64;

        Self {
            month,
            limit,
            daily_allowance,
            spent,
            daily_average,
            cells,
        }
    }

    /// 本月有没有超上限。
    pub fn over_limit(&self) -> bool {
        self.limit > 0 && self.spent > self.limit
    }

    /// 上限还剩多少（超了就是负数）。
    pub fn remaining(&self) -> AmountType {
        self.limit - self.spent
    }

    /// 已用比例，给进度条用；上限为 0 时给 0。
    pub fn used_ratio(&self) -> f64 {
        if self.limit <= 0 {
            return 0.0;
        }
        (self.spent as f64 / self.limit as f64).clamp(0.0, 1.0)
    }

    pub fn day(&self, day: u32) -> Option<&DayCell> {
        self.cells.get((day as usize).saturating_sub(1))
    }

    pub fn spent_on(&self, day: u32) -> AmountType {
        self.day(day).map(|cell| cell.spent).unwrap_or(0)
    }

    /// 到第 `day` 天为止的累计开销。
    pub fn cumulative_through(&self, day: u32) -> AmountType {
        let end = (day as usize).min(self.cells.len());
        self.cells[..end].iter().map(|cell| cell.spent).sum()
    }

    /// 到第 `day` 天为止应花的额度（日额度 × 天数）。
    pub fn allowance_through(&self, day: u32) -> AmountType {
        let days = (day as i64).min(self.cells.len() as i64).max(0);
        self.daily_allowance * days
    }

    /// 到第 `day` 天为止的额度结余：正数=还有余量，负数=透支。
    pub fn allowance_balance_through(&self, day: u32) -> AmountType {
        self.allowance_through(day) - self.cumulative_through(day)
    }
}

/// 按天汇总当月支出：跳过「不计入日历」的账单和非支出。
pub fn expense_by_day(month: NaiveDate, bills: &[BillEntity]) -> HashMap<u32, AmountType> {
    let mut by_day: HashMap<u32, AmountType> = HashMap::new();

    for bill in bills {
        if bill.excluded || bill.kind != BillKind::Expense {
            continue;
        }
        let date = bill.created_at.date_naive();
        if date.year() != month.year() || date.month() != month.month() {
            continue;
        }
        *by_day.entry(date.day()).or_default() += bill.amount;
    }

    by_day
}

/// 每个分类名（连同它的所有祖先）当月的实际支出。
///
/// 账单只打在最末一级分类上，但「餐饮」这种父分类要能看见「餐饮/外卖」的钱，
/// 所以一笔支出会沿 parent_of 往上加给每一级祖先。这样计划块绑父分类
/// 就能覆盖整个子树，绑子分类只对账那一条线。
///
/// 与 expense_by_day 同一口径：跳过「不计入日历」的账单和非支出。
pub fn expense_by_category(
    month: NaiveDate,
    bills: &[BillEntity],
    parent_of: &HashMap<String, String>,
) -> HashMap<String, AmountType> {
    let mut by_category: HashMap<String, AmountType> = HashMap::new();

    for bill in bills {
        if bill.excluded || bill.kind != BillKind::Expense {
            continue;
        }
        let date = bill.created_at.date_naive();
        if date.year() != month.year() || date.month() != month.month() {
            continue;
        }

        // 同一个名字只加一次：既防重复计入，也防脏数据里的父子环。
        let mut visited: Vec<String> = Vec::new();
        let mut current = bill.category.name.trim().to_string();
        while !current.is_empty() && !visited.iter().any(|seen| *seen == current) {
            *by_category.entry(current.clone()).or_default() += bill.amount;
            visited.push(current.clone());
            match parent_of.get(&current) {
                Some(parent) => current = parent.trim().to_string(),
                None => break,
            }
        }
    }

    by_category
}

/// 每一块本月已经花了多少：没绑分类的块给 None（不对账），
/// 绑了但一笔没花给 Some(0)。
pub fn spent_by_item(
    plan: &SpendingPlan,
    by_category: &HashMap<String, AmountType>,
) -> Vec<Option<AmountType>> {
    plan.items()
        .iter()
        .map(|item| {
            item.category
                .as_deref()
                .map(|category| by_category.get(category.trim()).copied().unwrap_or(0))
        })
        .collect()
}


/// 计划里每一块「本月已花」：没绑分类的块给 `None`，绑了就给该分类
/// （含子分类）的支出合计。分类树从 `categories` 现摊，调用方不用自己算父级。
pub fn spending_by_item(
    month: NaiveDate,
    bills: &[BillEntity],
    categories: &[Category],
    plan: &SpendingPlan,
) -> Vec<Option<AmountType>> {
    let parent_of = parent_names(categories);
    let by_category = expense_by_category(month, bills, &parent_of);
    spent_by_item(plan, &by_category)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bill_book::model::BillBook;
    use crate::category::model::Category;
    use chrono::{TimeZone, Utc};

    fn plan_of(pairs: &[(&str, AmountType)]) -> SpendingPlan {
        SpendingPlan::new(
            pairs
                .iter()
                .map(|(name, amount)| PlanItem::new(*name, *amount))
                .collect(),
        )
    }

    fn month() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
    }

    fn bill_at(
        id: u64,
        kind: BillKind,
        amount: AmountType,
        year: i32,
        month: u32,
        day: u32,
    ) -> BillEntity {
        BillEntity {
            id,
            kind,
            amount,
            book: BillBook {
                id: 1,
                name: "微信".to_string(),
            },
            category: Category {
                id: 1,
                parent_id: None,
                name: "测试".to_string(),
            },
            created_at: Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap(),
            remark: None,
            excluded: false,
        }
    }

    #[test]
    fn total_adds_every_bucket() {
        let plan = plan_of(&[("日常开销", 3_000_00), ("存款", 9_000_00), ("VPN", 50_00)]);
        assert_eq!(plan.total(), 12_050_00);
        assert_eq!(plan.limit(), plan.total());
        assert!(plan.is_set());
        assert!(!SpendingPlan::default().is_set());
        assert_eq!(SpendingPlan::default().total(), 0);
    }

    #[test]
    fn upsert_replaces_same_name_and_appends_new_one() {
        let mut plan = plan_of(&[("日常开销", 3_000_00)]);
        plan.upsert("日常开销", 3_500_00, None);
        plan.upsert("存款", 9_000_00, None);

        assert_eq!(plan.items().len(), 2);
        assert_eq!(plan.items()[0].amount, 3_500_00);
        assert_eq!(plan.total(), 12_500_00);
        assert_eq!(plan.position("存款"), Some(1));
        assert_eq!(plan.position("不存在"), None);
    }

    #[test]
    fn replace_and_remove_keep_the_order() {
        let mut plan = plan_of(&[("a", 1), ("b", 2), ("c", 3)]);
        plan.replace(1, "b2", 20, None);
        assert_eq!(plan.items()[1].name, "b2");
        assert_eq!(plan.items()[1].amount, 20);

        let removed = plan.remove(0).expect("第一块存在");
        assert_eq!(removed.name, "a");
        assert_eq!(plan.items().len(), 2);
        assert!(plan.remove(9).is_none());
    }

    #[test]
    fn validate_rejects_negative_duplicated_and_empty_names() {
        assert!(plan_of(&[("日常开销", 3_000_00)]).validate().is_ok());
        assert!(
            SpendingPlan::default().validate().is_ok(),
            "空计划是合法起点"
        );

        assert!(plan_of(&[("a", -1)]).validate().is_err());
        assert!(plan_of(&[("", 1)]).validate().is_err());
        assert!(plan_of(&[("a", 1), ("a", 2)]).validate().is_err());
        assert!(plan_of(&[("a\u{7}", 1)]).validate().is_err());
    }

    #[test]
    fn validate_caps_the_number_of_buckets() {
        let items = (0..=SpendingPlan::MAX_ITEMS)
            .map(|index| PlanItem::new(format!("块{index}"), 1))
            .collect::<Vec<_>>();
        assert!(SpendingPlan::new(items).validate().is_err());
    }

    #[test]
    fn coverage_compares_total_with_income() {
        let plan = plan_of(&[("日常开销", 3_000_00), ("存款", 9_000_00)]);

        let ok = PlanCoverage::new(12_500_00, &plan);
        assert!(ok.covers());
        assert_eq!(ok.remaining(), 500_00);
        assert_eq!(ok.shortfall(), 0);
        assert!((ok.ratio() - 0.96).abs() < 1e-9);

        let tight = PlanCoverage::new(12_000_00, &plan);
        assert!(tight.covers(), "刚好分完也算覆盖");

        let over = PlanCoverage::new(10_000_00, &plan);
        assert!(!over.covers());
        assert_eq!(over.shortfall(), 2_000_00);
        assert_eq!(over.ratio(), 1.2);
    }

    #[test]
    fn coverage_without_income_only_accepts_empty_plan() {
        assert!(PlanCoverage::new(0, &SpendingPlan::default()).covers());
        assert!(!PlanCoverage::new(0, &plan_of(&[("存款", 100)])).covers());
        assert_eq!(
            PlanCoverage::new(0, &plan_of(&[("存款", 100)])).ratio(),
            0.0
        );
    }

    #[test]
    fn month_income_counts_only_income_in_that_month() {
        let bills = vec![
            bill_at(1, BillKind::Income, 12_000_00, 2026, 10, 5),
            bill_at(2, BillKind::Income, 500_00, 2026, 10, 20),
            bill_at(3, BillKind::Expense, 300_00, 2026, 10, 6),
            bill_at(4, BillKind::Income, 9_000_00, 2026, 9, 30),
        ];
        assert_eq!(month_income(month(), &bills), 12_500_00);
    }

    #[test]
    fn days_in_month_handles_february() {
        assert_eq!(
            days_in_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()),
            28
        );
        assert_eq!(
            days_in_month(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap()),
            29
        );
        assert_eq!(days_in_month(month()), 31);
    }

    #[test]
    fn expense_by_day_skips_excluded_and_income() {
        let mut excluded = bill_at(2, BillKind::Expense, 500_00, 2026, 10, 3);
        excluded.excluded = true;
        let bills = vec![
            bill_at(1, BillKind::Expense, 200_00, 2026, 10, 3),
            excluded,
            bill_at(3, BillKind::Income, 9_000_00, 2026, 10, 3),
            bill_at(4, BillKind::Expense, 700_00, 2026, 11, 1),
        ];

        let by_day = expense_by_day(month(), &bills);
        assert_eq!(by_day.get(&3).copied(), Some(200_00));
        assert_eq!(by_day.len(), 1);
    }

    #[test]
    fn progress_spreads_the_plan_over_the_month() {
        let plan = plan_of(&[("日常开销", 3_100_00)]);
        let mut spending = HashMap::new();
        spending.insert(1, 100_00);
        spending.insert(2, 200_00);

        let progress = PlanProgress::build(month(), &plan, None, &spending);
        assert_eq!(progress.limit, 3_100_00);
        assert_eq!(progress.daily_allowance, 100_00);
        assert_eq!(progress.spent, 300_00);
        assert_eq!(progress.daily_average, 300_00 / 31);
        assert_eq!(progress.allowance_through(3), 300_00);
        assert_eq!(progress.cumulative_through(2), 300_00);
        assert_eq!(progress.allowance_balance_through(3), 0);
        assert!((progress.used_ratio() - 0.0967).abs() < 0.001);
    }

    #[test]
    fn progress_marks_days_that_blow_the_daily_allowance() {
        let plan = plan_of(&[("日常开销", 3_100_00)]);
        let mut spending = HashMap::new();
        spending.insert(1, 50_00);
        spending.insert(2, 400_00);

        let progress = PlanProgress::build(month(), &plan, None, &spending);
        assert_eq!(progress.day(1).unwrap().status, DayStatus::WithinDaily);
        assert_eq!(progress.day(2).unwrap().status, DayStatus::OverDaily);
        assert_eq!(progress.day(3).unwrap().status, DayStatus::None);
    }

    #[test]
    fn progress_flags_the_whole_month_once_the_limit_is_passed() {
        let plan = plan_of(&[("日常开销", 100_00)]);
        let mut spending = HashMap::new();
        spending.insert(1, 80_00);
        spending.insert(2, 80_00);

        let progress = PlanProgress::build(month(), &plan, None, &spending);
        assert!(progress.over_limit());
        assert_eq!(progress.remaining(), -60_00);
        // 第 2 天累计 160 > 日额度(3) * 2，直接标红；第 1 天只是超日额度
        assert_eq!(progress.day(2).unwrap().status, DayStatus::OverLimit);
        assert_eq!(progress.day(1).unwrap().status, DayStatus::OverDaily);
    }

    #[test]
    fn progress_without_plan_keeps_the_calendar_neutral() {
        let progress =
            PlanProgress::build(month(), &SpendingPlan::default(), None, &HashMap::new());
        assert_eq!(progress.limit, 0);
        assert_eq!(progress.daily_allowance, 0);
        assert!(!progress.over_limit());
        assert_eq!(progress.used_ratio(), 0.0);
        assert!(
            progress
                .cells
                .iter()
                .all(|cell| cell.status == DayStatus::None)
        );
    }

    #[test]
    fn progress_average_divides_by_elapsed_days_when_viewing_this_month() {
        let plan = plan_of(&[("日常开销", 3_100_00)]);
        let mut spending = HashMap::new();
        spending.insert(1, 100_00);
        spending.insert(2, 100_00);

        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let progress = PlanProgress::build(month(), &plan, Some(today), &spending);
        assert_eq!(progress.daily_average, 200_00 / 10);

        // 查看别的月份时 today 不落在当月，分母退回整月天数
        let other = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let progress = PlanProgress::build(other, &plan, Some(today), &spending);
        assert_eq!(progress.daily_average, 200_00 / 30);
        assert_eq!(progress.cells.len(), 30);
    }

    #[test]
    fn binding_a_category_rolls_child_spending_into_the_parent() {
        // 餐饮（父）→ 外卖（子）。账单只打在子分类上，
        // 绑父分类的块要能看见整棵子树的钱。
        let categories = vec![
            Category {
                id: 1,
                parent_id: None,
                name: "餐饮".to_string(),
            },
            Category {
                id: 2,
                parent_id: Some(1),
                name: "外卖".to_string(),
            },
        ];

        let mut takeout = bill_at(1, BillKind::Expense, 30_00, 2026, 10, 3);
        takeout.category = categories[1].clone();
        let mut outside = bill_at(2, BillKind::Expense, 70_00, 2026, 10, 4);
        outside.excluded = true;

        let plan = SpendingPlan::new(vec![
            PlanItem::new("吃饭", 1_000_00).with_category(Some("餐饮")),
            PlanItem::new("外卖专款", 400_00).with_category(Some("外卖")),
            PlanItem::new("存款", 9_000_00),
        ]);

        let spent = spending_by_item(month(), &[takeout, outside], &categories, &plan);
        assert_eq!(spent[0], Some(30_00), "父分类要含子分类的支出");
        assert_eq!(spent[1], Some(30_00), "子分类只算自己这条线");
        assert_eq!(spent[2], None, "没绑分类的块不对账");
    }

    #[test]
    fn bound_category_without_bills_reports_zero_not_none() {
        let categories = vec![Category {
            id: 1,
            parent_id: None,
            name: "交通".to_string(),
        }];
        let plan = SpendingPlan::new(vec![PlanItem::new("通勤", 300_00).with_category(Some("交通"))]);

        let spent = spending_by_item(month(), &[], &categories, &plan);
        assert_eq!(spent, vec![Some(0)], "绑了没花是 0，不是没对账");
    }

    #[test]
    fn one_category_cannot_fund_two_buckets() {
        let plan = SpendingPlan::new(vec![
            PlanItem::new("吃饭", 800_00).with_category(Some("餐饮")),
            PlanItem::new("聚餐", 200_00).with_category(Some("餐饮")),
        ]);
        let error = plan.validate().expect_err("同一分类绑两块要拦下");
        assert!(error.contains("餐饮"), "报错要点名是哪个分类：{error}");
    }
}
