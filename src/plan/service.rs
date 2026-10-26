//! 资金计划的业务层：校验后落库，或按当月账单算执行情况与覆盖情况。
//!
//! 跨模块只认这一层：TUI / CLI 都通过 [`PlanService`] 拿计划、进度与覆盖。

use anyhow::{Result, anyhow, bail};
use chrono::NaiveDate;

use crate::bill::model::BillEntity;
use crate::bill::service::BillService;
use crate::category::service::CategoryService;
use crate::common::db::Pool;
use crate::common::types::AmountType;
use crate::plan::dao::PlanDao;
use crate::plan::model::{
    PlanCoverage, PlanProgress, SpendingPlan, expense_by_day, month_income, spending_by_item,
};

#[derive(Clone)]
pub struct PlanService {
    dao: PlanDao,
    /// 只给 CLI 的 `plan check` / `plan show` 用：那两条命令要自己查当月账单。
    bills: BillService,
    /// `plan show` 要把「已花」算出来，得知道分类的父子关系。
    categories: CategoryService,
}

impl PlanService {
    pub fn new(pool: Pool) -> Self {
        Self {
            dao: PlanDao::new(pool.clone()),
            bills: BillService::new(pool.clone()),
            categories: CategoryService::new(pool),
        }
    }

    /// 读取当前计划。
    pub async fn get(&self) -> Result<SpendingPlan> {
        self.dao.get().await
    }

    /// 保存计划；先过一遍 [`SpendingPlan::validate`]，别把自相矛盾的数存进去。
    pub async fn save(&self, plan: SpendingPlan) -> Result<()> {
        plan.validate().map_err(|message| anyhow!("{message}"))?;
        self.dao.save(&plan).await
    }

    /// 新增或改一块（同名覆盖）。`category` 传 `None` 表示「沿用原来的绑定」——
    /// 改金额不该顺手把分类解绑；要解绑请显式传空串（见 `unbind`）。
    pub async fn set_item(
        &self,
        name: &str,
        amount: AmountType,
        category: Option<&str>,
    ) -> Result<SpendingPlan> {
        let mut plan = self.get().await?;
        if !plan.is_set() {
            bail!("计划还是空的，先新增第一块");
        }
        let category = match category {
            Some(category) => Some(category.to_string()),
            None => plan
                .position(name)
                .and_then(|index| plan.get(index))
                .and_then(|item| item.category.clone()),
        };
        plan.upsert(name, amount, category.as_deref());
        self.save(plan.clone()).await?;
        Ok(plan)
    }

    /// 新增一块；已经有同名块时直接拒绝，免得「加」变成了「改」。
    pub async fn add_item(
        &self,
        name: &str,
        amount: AmountType,
        category: Option<&str>,
    ) -> Result<SpendingPlan> {
        let mut plan = self.get().await?;
        if plan.position(name).is_some() {
            bail!("「{name}」已经在计划里了：改金额用 plan set --name，或在计划页选中它按 e");
        }
        plan.upsert(name, amount, category);
        self.save(plan.clone()).await?;
        Ok(plan)
    }

    /// 按位置改一块：位置就是展示顺序，改名也落在原地。
    /// 这里的 `category` 是最终值：`None` 就是解绑（表单里的「(不绑定)」）。
    pub async fn update_item(
        &self,
        index: usize,
        name: &str,
        amount: AmountType,
        category: Option<&str>,
    ) -> Result<SpendingPlan> {
        let mut plan = self.get().await?;
        if plan.get(index).is_none() {
            bail!("计划里没有第 {} 块", index + 1);
        }
        plan.replace(index, name, amount, category);
        self.save(plan.clone()).await?;
        Ok(plan)
    }

    /// 删掉一块。
    pub async fn remove_item(&self, name: &str) -> Result<SpendingPlan> {
        let mut plan = self.get().await?;
        let index = plan
            .position(name)
            .ok_or_else(|| anyhow!("计划里没有「{name}」这一块"))?;
        plan.remove(index);
        self.save(plan.clone()).await?;
        Ok(plan)
    }

    /// 清空计划。
    pub async fn clear(&self) -> Result<()> {
        self.save(SpendingPlan::default()).await
    }

    /// 按当月账单算执行情况；账单里 `excluded` 的那些会被跳过。
    pub fn progress(
        &self,
        month: NaiveDate,
        plan: &SpendingPlan,
        today: Option<NaiveDate>,
        bills: &[BillEntity],
    ) -> PlanProgress {
        let by_day = expense_by_day(month, bills);
        PlanProgress::build(month, plan, today, &by_day)
    }

    /// 计划合计 vs 当月收入。账单已经取好时用它，省一次查询。
    pub fn coverage(
        &self,
        month: NaiveDate,
        plan: &SpendingPlan,
        bills: &[BillEntity],
    ) -> PlanCoverage {
        PlanCoverage::new(month_income(month, bills), plan)
    }

    /// 同上，但自己去查当月账单——CLI 的 `plan check` 走这条路。
    pub async fn coverage_for(&self, month: NaiveDate) -> Result<PlanCoverage> {
        let plan = self.get().await?;
        let bills = self.bills.list_in_month(month).await?;
        Ok(self.coverage(month, &plan, &bills))
    }

    /// 每一块本月已花：没绑分类的块是 `None`。账单已经取好时用它，省一次查询。
    pub async fn spending_with(
        &self,
        month: NaiveDate,
        plan: &SpendingPlan,
        bills: &[BillEntity],
    ) -> Result<Vec<Option<AmountType>>> {
        if !plan.items().iter().any(|item| item.is_bound()) {
            return Ok(vec![None; plan.items().len()]);
        }
        let nodes = self.categories.tree().await?;
        let categories = nodes.into_iter().map(|node| node.category).collect::<Vec<_>>();
        Ok(spending_by_item(month, bills, &categories, plan))
    }

    /// 同上，但自己去查当月账单——CLI 的 `plan show` 走这条路。
    pub async fn spending(
        &self,
        month: NaiveDate,
        plan: &SpendingPlan,
    ) -> Result<Vec<Option<AmountType>>> {
        let bills = self.bills.list_in_month(month).await?;
        self.spending_with(month, plan, &bills).await
    }
}
