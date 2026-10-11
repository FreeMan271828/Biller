//! 开销计划的业务层：校验后落库，或按当月账单算执行情况。
//!
//! 跨模块只认这一层：TUI / CLI 都通过 [`PlanService`] 拿计划和进度。

use anyhow::{Result, bail};
use chrono::NaiveDate;

use crate::bill::model::BillEntity;
use crate::common::db::Pool;
use crate::plan::dao::PlanDao;
use crate::plan::model::{PlanProgress, SpendingPlan, expense_by_day};

#[derive(Clone)]
pub struct PlanService {
    dao: PlanDao,
}

impl PlanService {
    pub fn new(pool: Pool) -> Self {
        Self {
            dao: PlanDao::new(pool),
        }
    }

    /// 读取当前计划。
    pub async fn get(&self) -> Result<SpendingPlan> {
        self.dao.get().await
    }

    /// 保存计划；攒钱超过预计直接拒绝，免得存进一个自相矛盾的数。
    pub async fn save(&self, plan: SpendingPlan) -> Result<()> {
        if plan.expected < 0 {
            bail!("预计金额不能为负");
        }
        if plan.savings < 0 {
            bail!("攒钱金额不能为负");
        }
        if plan.savings > plan.expected {
            bail!("攒钱金额不能超过预计金额");
        }

        self.dao.save(&plan).await
    }

    /// 按当月账单算执行情况；账单里 `excluded` 的那些会被跳过。
    pub fn progress(
        &self,
        month: NaiveDate,
        plan: SpendingPlan,
        today: Option<NaiveDate>,
        bills: &[BillEntity],
    ) -> PlanProgress {
        let by_day = expense_by_day(month, bills);
        PlanProgress::build(month, plan, today, &by_day)
    }
}
