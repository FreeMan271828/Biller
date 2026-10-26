//! 计划的终端交互层（CLI）：只做参数解析与输出。

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, NaiveDate};
use clap::Subcommand;

use crate::common::money;
use crate::common::table::{display_width, pad_left, pad_right};
use crate::common::time;
use crate::plan::model::{PlanCoverage, PlanRow};
use crate::plan::service::PlanService;

#[derive(Debug, Subcommand)]
pub enum PlanCmd {
    /// 查看计划：每个计划的上限、本月已花、剩余，以及收入能不能覆盖
    Show {
        /// 要对比的月份，例如 2026-10；默认本月
        #[arg(long)]
        month: Option<String>,
    },
    /// 新建计划；不传 --limit 就是「只累计、不设上限」
    Add {
        /// 计划名，例如 房租
        #[arg(long)]
        name: String,
        /// 每月额度（元），例如 3000；不传表示不设上限
        #[arg(long)]
        limit: Option<String>,
    },
    /// 改某个计划的上限；传 - 表示取消上限（只累计）
    Set {
        /// 计划名
        #[arg(long)]
        name: String,
        /// 每月额度（元），或者 - 取消上限
        #[arg(long)]
        limit: String,
    },
    /// 删掉一个计划（名下还有账单时会被拒）
    Remove {
        /// 计划名
        #[arg(long)]
        name: String,
    },
    /// 只做覆盖检查：有上限的计划合计 vs 该月收入
    Check {
        /// 要对比的月份，例如 2026-10；默认本月
        #[arg(long)]
        month: Option<String>,
    },
}

pub async fn run(service: &PlanService, cmd: PlanCmd) -> Result<()> {
    match cmd {
        PlanCmd::Show { month } => {
            let month = month_of(month.as_deref())?;
            let rows = service.rows(month).await?;
            print_plan(&rows, month);
            println!();
            let coverage = service.snapshot(month, None).await?.coverage;
            print_coverage(month, &coverage);
        }
        PlanCmd::Add { name, limit } => {
            let limit = limit_of(limit.as_deref())?;
            let plan = service.add(&name, limit).await?;
            println!(
                "已新增计划「{}」{}",
                plan.name(),
                limit_suffix(plan.limit)
            );
        }
        PlanCmd::Set { name, limit } => {
            let limit = limit_of(Some(&limit))?;
            let plan = service.set_limit(&name, limit).await?;
            println!(
                "计划「{}」{}",
                plan.name(),
                limit_suffix(plan.limit)
            );
        }
        PlanCmd::Remove { name } => {
            let plan = service.remove(&name).await?;
            println!("已删除计划「{}」", plan.name());
        }
        PlanCmd::Check { month } => {
            let month = month_of(month.as_deref())?;
            let coverage = service.snapshot(month, None).await?.coverage;
            print_coverage(month, &coverage);
            if !coverage.covers() {
                // 退出码留给人手的脚本用：分超了就是失败
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// `--limit` 的取值：不传 = 没上限；`-` / `none` / `不设` = 显式取消上限。
fn limit_of(value: Option<&str>) -> Result<Option<crate::common::types::AmountType>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() || value == "-" || value.eq_ignore_ascii_case("none") || value == "不设" {
        return Ok(None);
    }
    let amount = money::parse_amount(value)?;
    if amount <= 0 {
        bail!("上限要大于 0；不想设上限就别传 --limit");
    }
    Ok(Some(amount))
}

/// 命令行回执里的「 · 每月 3,000.00 元 / · 不设上限」。
fn limit_suffix(limit: Option<crate::common::types::AmountType>) -> String {
    match limit {
        Some(limit) => format!(" · 每月 {} 元", money::format_amount(limit)),
        None => " · 不设上限，只累计".to_string(),
    }
}

/// 金额列宽度：够放「12,345.00」这种量级，再长就按实际撑开。
const MONEY_WIDTH: usize = 12;

fn print_plan(rows: &[PlanRow], month: NaiveDate) {
    if rows.is_empty() {
        println!("还没有计划。先建一个：biller plan add --name 房租 --limit 3000");
        return;
    }

    // 中文占两列，必须按显示宽度对齐，不能按字符数
    let name_width = rows
        .iter()
        .map(|row| display_width(row.name()))
        .max()
        .unwrap_or(0)
        .max(8);

    println!("计划  {}", time::format_month(month));
    println!(
        "  {}  {}  {}  {}",
        pad_right("名称", name_width),
        pad_left("上限", MONEY_WIDTH),
        pad_left("已花", MONEY_WIDTH),
        pad_left("剩余", MONEY_WIDTH)
    );
    print_rule(name_width);

    for row in rows {
        let limit_text = match row.plan.limit {
            Some(limit) => money::format_amount(limit),
            None => "不限".to_string(),
        };
        let left_text = match row.remaining() {
            Some(rest) => money::format_amount(rest),
            None => "—".to_string(),
        };
        println!(
            "  {}  {}  {}  {}",
            pad_right(row.name(), name_width),
            pad_left(&limit_text, MONEY_WIDTH),
            pad_left(&money::format_amount(row.spent), MONEY_WIDTH),
            pad_left(&left_text, MONEY_WIDTH)
        );
    }

    print_rule(name_width);
    let capped: Vec<&PlanRow> = rows.iter().filter(|row| row.plan.has_limit()).collect();
    let limit_total: crate::common::types::AmountType =
        capped.iter().filter_map(|row| row.plan.limit).sum();
    let spent_total = rows.iter().map(|row| row.spent).sum();
    println!(
        "  {}  {}  {}  {}",
        pad_right("合计", name_width),
        pad_left(&money::format_amount(limit_total), MONEY_WIDTH),
        pad_left(&money::format_amount(spent_total), MONEY_WIDTH),
        pad_left(&money::format_amount(limit_total - spent_total), MONEY_WIDTH)
    );

    if capped.len() < rows.len() {
        println!("（不限 = 不设上限，只累计花掉多少，不判超支）");
    }
}

fn print_rule(name_width: usize) {
    println!(
        "  {}  {}  {}  {}",
        "─".repeat(name_width),
        "─".repeat(MONEY_WIDTH),
        "─".repeat(MONEY_WIDTH),
        "─".repeat(MONEY_WIDTH)
    );
}

fn print_coverage(month: NaiveDate, coverage: &PlanCoverage) {
    println!(
        "{} 收入 {} · 有上限的计划 {}",
        time::format_month(month),
        money::format_amount(coverage.income),
        money::format_amount(coverage.allocated)
    );

    if coverage.income <= 0 {
        // 没有收入就没有分母，别拿 0.0% 糊弄人
        println!("覆盖：· 这个月还没有收入账单，先记一笔工资再校验");
    } else if coverage.covers() {
        println!(
            "覆盖：✓ 还剩 {} 元没分配（计划占收入 {:.1}%）",
            money::format_amount(coverage.remaining()),
            coverage.ratio() * 100.0
        );
    } else {
        println!(
            "覆盖：✗ 分配超了收入 {} 元（计划占收入 {:.1}%）",
            money::format_amount(coverage.shortfall()),
            coverage.ratio() * 100.0
        );
    }
}

/// `--month 2026-10` → 当月 1 号；不传就是本月。
fn month_of(value: Option<&str>) -> Result<NaiveDate> {
    let Some(value) = value else {
        return Ok(time::first_day_of_month(chrono::Local::now().date_naive()));
    };

    let parsed = NaiveDate::parse_from_str(&format!("{}-01", value.trim()), "%Y-%m-%d")
        .map_err(|_| anyhow!("月份要写成 2026-10 这种格式，收到：{value}"))?;

    // 统一用当月 1 号表示，和数据库里存的月份键一致
    Ok(NaiveDate::from_ymd_opt(parsed.year(), parsed.month(), 1).unwrap_or(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_dash_clears_the_limit() {
        assert_eq!(limit_of(Some("-")).expect("可以用"), None);
        assert_eq!(limit_of(Some(" 不设 ")).expect("可以用"), None);
        assert_eq!(limit_of(Some("NONE")).expect("可以用"), None);
        assert_eq!(limit_of(None).expect("可以用"), None);
    }

    #[test]
    fn limit_parses_and_rejects_non_positive() {
        assert_eq!(limit_of(Some("3000")).expect("可以用"), Some(3000_00));
        assert!(limit_of(Some("0")).is_err());
        assert!(limit_of(Some("-5")).is_err());
    }

    #[test]
    fn suffix_explains_capped_and_uncapped() {
        assert_eq!(limit_suffix(Some(3000_00)), " · 每月 3,000.00 元");
        assert_eq!(limit_suffix(None), " · 不设上限，只累计");
    }
}
