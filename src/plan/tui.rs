//! 开销计划的终端交互层（CLI）：只做参数解析与输出。

use anyhow::{Result, anyhow};
use clap::Subcommand;

use crate::common::money;
use crate::plan::model::{PlanMode, SpendingPlan};
use crate::plan::service::PlanService;

#[derive(Debug, Subcommand)]
pub enum PlanCmd {
    /// 查看当前计划
    Show,
    /// 设置计划（只改给出的项，另一项保持原值）
    Set {
        /// 预计金额（元），例如 12000
        #[arg(long)]
        expected: Option<String>,
        /// 攒钱金额（元），例如 8000
        #[arg(long)]
        savings: Option<String>,
        /// 超支判断：cumulative（累计标红，默认）或 daily（只看当天）
        #[arg(long)]
        mode: Option<String>,
    },
}

pub async fn run(service: &PlanService, cmd: PlanCmd) -> Result<()> {
    match cmd {
        PlanCmd::Show => {
            let plan = service.get().await?;
            if !plan.is_set() {
                println!("还没有设置开销计划；用 `biller plan set --expected 12000 --savings 8000` 设置");
                return Ok(());
            }
            println!("预计金额：{} 元", money::format_amount(plan.expected));
            println!("攒钱金额：{} 元", money::format_amount(plan.savings));
            println!("上限开销：{} 元", money::format_amount(plan.limit()));
        }
        PlanCmd::Set {
            expected,
            savings,
            mode,
        } => {
            let current = service.get().await?;
            let next = SpendingPlan::new(
                expected
                    .as_deref()
                    .map(money::parse_amount)
                    .transpose()?
                    .unwrap_or(current.expected),
                savings
                    .as_deref()
                    .map(money::parse_amount)
                    .transpose()?
                    .unwrap_or(current.savings),
            )
            .with_mode(match mode.as_deref() {
                Some(value) => PlanMode::parse(value)
                    .ok_or_else(|| anyhow!("超支判断只支持 cumulative / daily，收到：{value}"))?,
                None => current.mode,
            });

            service.save(next).await?;
            println!(
                "计划已更新：预计 {} 元 · 攒钱 {} 元 · 上限开销 {} 元/月 · {}",
                money::format_amount(next.expected),
                money::format_amount(next.savings),
                money::format_amount(next.limit()),
                next.mode.label()
            );
        }
    }

    Ok(())
}
