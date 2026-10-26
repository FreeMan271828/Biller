//! 资金计划的终端交互层（CLI）：只做参数解析与输出。

use anyhow::{Result, anyhow};
use chrono::{Datelike, NaiveDate};
use clap::Subcommand;

use crate::common::money;
use crate::common::table::{display_width, pad_left, pad_right};
use crate::common::time;
use crate::common::types::AmountType;
use crate::plan::model::{PlanCoverage, SpendingPlan};
use crate::plan::service::PlanService;

#[derive(Debug, Subcommand)]
pub enum PlanCmd {
    /// 查看计划：每一块 + 合计 + 本月收入能不能覆盖
    Show {
        /// 要对比的月份，例如 2026-10；默认本月
        #[arg(long)]
        month: Option<String>,
    },
    /// 新增一块（已经有同名块时报错，改金额用 set）
    Add {
        /// 这一块的名字，例如 日常开销
        #[arg(long)]
        name: String,
        /// 每月金额（元），例如 3000
        #[arg(long)]
        amount: String,
        /// 绑定一个支出分类（可选）：绑了以后计划会自动显示这一块本月已花/剩余，
        /// 分类的子分类也算进来。传 - 表示不绑定
        #[arg(long)]
        category: Option<String>,
    },
    /// 新增或修改一块（同名覆盖）
    Set {
        /// 这一块的名字，例如 存款
        #[arg(long)]
        name: String,
        /// 每月金额（元），例如 9000
        #[arg(long)]
        amount: String,
        /// 绑定 / 换绑分类；不传就沿用原来的绑定，传 - 解绑
        #[arg(long)]
        category: Option<String>,
    },
    /// 删掉一块
    Remove {
        /// 这一块的名字
        #[arg(long)]
        name: String,
    },
    /// 清空整个计划
    Clear,
    /// 只做覆盖检查：计划合计 vs 该月收入
    Check {
        /// 要对比的月份，例如 2026-10；默认本月
        #[arg(long)]
        month: Option<String>,
    },
}

pub async fn run(service: &PlanService, cmd: PlanCmd) -> Result<()> {
    match cmd {
        PlanCmd::Show { month } => {
            let plan = service.get().await?;
            let month = month_of(month.as_deref())?;
            let spent = service.spending(month, &plan).await?;

            print_plan(&plan, month, &spent);
            println!();
            let coverage = service.coverage_for(month).await?;
            print_coverage(month, &coverage);
        }
        PlanCmd::Add {
            name,
            amount,
            category,
        } => {
            let amount = money::parse_amount(&amount)?;
            let category = category_flag(category.as_deref());
            let plan = service
                .add_item(&name, amount, category.as_deref())
                .await?;
            println!(
                "已新增「{}」{} 元/月{} · 计划合计 {} 元",
                name.trim(),
                money::format_amount(amount),
                binding_suffix(category.as_deref()),
                money::format_amount(plan.total())
            );
        }
        PlanCmd::Set {
            name,
            amount,
            category,
        } => {
            let amount = money::parse_amount(&amount)?;
            let category = category_flag(category.as_deref());
            let plan = service
                .set_item(&name, amount, category.as_deref())
                .await?;
            println!(
                "「{}」已设为 {} 元/月{} · 计划合计 {} 元",
                name.trim(),
                money::format_amount(amount),
                binding_suffix(category.as_deref()),
                money::format_amount(plan.total())
            );
        }
        PlanCmd::Remove { name } => {
            let plan = service.remove_item(&name).await?;
            println!(
                "已删除「{}」 · 计划合计 {} 元",
                name.trim(),
                money::format_amount(plan.total())
            );
        }
        PlanCmd::Clear => {
            service.clear().await?;
            println!("计划已清空");
        }
        PlanCmd::Check { month } => {
            let month = month_of(month.as_deref())?;
            let coverage = service.coverage_for(month).await?;
            print_coverage(month, &coverage);
            if !coverage.covers() {
                // 退出码留给人手的脚本用：分超了就是失败
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// `--category` 的取值：`-` / `none` / `不绑定` 都当作解绑（空串 = 不绑定）。
fn category_flag(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value == "-" || value.eq_ignore_ascii_case("none") || value == "不绑定"
    {
        return Some(String::new());
    }
    Some(value.to_string())
}

/// 命令行回执里的「 · 绑定分类：餐饮」尾巴。
fn binding_suffix(category: Option<&str>) -> String {
    match category.map(str::trim) {
        Some(name) if !name.is_empty() => format!(" · 绑定分类：{name}"),
        Some(_) => " · 已解绑分类".to_string(),
        None => String::new(),
    }
}

/// 金额列宽度：够放「12,345.00」这种量级，再长就按实际撑开。
const MONEY_WIDTH: usize = 12;

fn print_plan(plan: &SpendingPlan, month: NaiveDate, spent: &[Option<AmountType>]) {
    if !plan.is_set() {
        println!("还没有计划。先分第一块：biller plan add --name 日常开销 --amount 3000");
        return;
    }

    // 中文占两列，必须按显示宽度对齐，不能按字符数
    let name_width = plan
        .items()
        .iter()
        .map(|item| display_width(&item.name))
        .max()
        .unwrap_or(0)
        .max(8);

    let spent_of = |index: usize| spent.get(index).copied().flatten();

    println!("计划（每月）  {}", time::format_month(month));
    println!(
        "  {}  {}  {}  {}",
        pad_right("名称", name_width),
        pad_left("额度", MONEY_WIDTH),
        pad_left("已花", MONEY_WIDTH),
        pad_left("剩余", MONEY_WIDTH)
    );
    println!(
        "  {}  {}  {}  {}",
        "─".repeat(name_width),
        "─".repeat(MONEY_WIDTH),
        "─".repeat(MONEY_WIDTH),
        "─".repeat(MONEY_WIDTH)
    );

    for (index, item) in plan.items().iter().enumerate() {
        let (spent_text, left_text) = match spent_of(index) {
            Some(used) => (
                money::format_amount(used),
                money::format_amount(item.amount - used),
            ),
            None => ("—".to_string(), "—".to_string()),
        };
        println!(
            "  {}  {}  {}  {}",
            pad_right(&item.name, name_width),
            pad_left(&money::format_amount(item.amount), MONEY_WIDTH),
            pad_left(&spent_text, MONEY_WIDTH),
            pad_left(&left_text, MONEY_WIDTH)
        );
    }

    let bound: Vec<usize> = (0..plan.items().len())
        .filter(|index| spent_of(*index).is_some())
        .collect();
    let spent_total: AmountType = bound.iter().filter_map(|index| spent_of(*index)).sum();
    let amount_of_bound: AmountType = bound.iter().map(|index| plan.items()[*index].amount).sum();

    println!(
        "  {}  {}  {}  {}",
        "─".repeat(name_width),
        "─".repeat(MONEY_WIDTH),
        "─".repeat(MONEY_WIDTH),
        "─".repeat(MONEY_WIDTH)
    );
    println!(
        "  {}  {}  {}  {}",
        pad_right("合计", name_width),
        pad_left(&money::format_amount(plan.total()), MONEY_WIDTH),
        pad_left(&money::format_amount(spent_total), MONEY_WIDTH),
        pad_left(&money::format_amount(amount_of_bound - spent_total), MONEY_WIDTH)
    );

    if bound.is_empty() {
        println!("（还没绑分类：想自动对账就给某块加 --category 分类名）");
    } else if bound.len() < plan.items().len() {
        println!("（— 表示没绑分类，只占额度、不跟账单对账）");
    }
}

fn print_coverage(month: NaiveDate, coverage: &PlanCoverage) {
    println!(
        "{} 收入 {} · 计划 {}",
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
    fn bare_dash_unbinds_the_category() {
        assert_eq!(category_flag(Some("-")), Some(String::new()));
        assert_eq!(category_flag(Some(" 不绑定 ")), Some(String::new()));
        assert_eq!(category_flag(Some("NONE")), Some(String::new()));
        assert_eq!(category_flag(None), None);
    }

    #[test]
    fn category_and_suffix_round_trip() {
        assert_eq!(category_flag(Some(" 餐饮 ")), Some("餐饮".to_string()));
        assert_eq!(binding_suffix(Some("餐饮")), " · 绑定分类：餐饮");
        assert_eq!(binding_suffix(Some("  ")), " · 已解绑分类");
        assert_eq!(binding_suffix(None), "");
    }
}