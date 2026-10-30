//! 计划的终端交互层（CLI）：只做参数解析与输出。

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, NaiveDate};
use clap::Subcommand;

use crate::bill_book::model::PlanKind;
use crate::common::money;
use crate::common::table::{display_width, pad_left, pad_right};
use crate::common::time;
use crate::plan::model::{PlanCoverage, PlanRow};
use crate::plan::service::PlanService;

#[derive(Debug, Subcommand)]
pub enum PlanCmd {
    /// 查看计划：收入在前、开销在后，各看各的额度与本月的进度
    Show {
        /// 要对比的月份，例如 2026-10；默认本月
        #[arg(long)]
        month: Option<String>,
    },
    /// 新建计划；不传 --limit 就是「只累计、不设额度」
    Add {
        /// 计划名，例如 房租
        #[arg(long)]
        name: String,
        /// 每月额度（元），例如 3000；不传表示不设额度
        #[arg(long)]
        limit: Option<String>,
        /// 计划类型：income（收入，如工资）/ expense（开销，如房租）
        #[arg(long, default_value = "expense")]
        kind: String,
    },
    /// 改某个计划的额度或类型；--limit 传 - 表示取消额度，两个都不给就没什么可改
    Set {
        /// 计划名
        #[arg(long)]
        name: String,
        /// 每月额度（元），或者 - 取消额度；不传表示不改额度
        #[arg(long)]
        limit: Option<String>,
        /// 改成 income（收入）或 expense（开销）
        #[arg(long)]
        kind: Option<String>,
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
        PlanCmd::Add { name, limit, kind } => {
            let limit = limit_of(limit.as_deref())?;
            let plan = service.add(&name, limit, kind_of(&kind)?).await?;
            println!(
                "已新增{}计划「{}」 · {}",
                plan.kind.label(),
                plan.name(),
                plan.describe_limit()
            );
        }
        PlanCmd::Set { name, limit, kind } => {
            if limit.is_none() && kind.is_none() {
                bail!("至少给 --limit 或 --kind：一个都不给就没什么可改的");
            }
            let mut plan = service.get(&name).await?;
            if let Some(limit) = limit.as_deref() {
                plan = service.set_limit(&name, limit_of(Some(limit))?).await?;
            }
            if let Some(kind) = kind.as_deref() {
                plan = service.set_kind(&name, kind_of(kind)?).await?;
            }
            println!(
                "计划「{}」已改为{}计划 · {}",
                plan.name(),
                plan.kind.label(),
                plan.describe_limit()
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

/// `--kind` 的取值：英文 income / expense，也认中文「收入 / 开销」。
fn kind_of(value: &str) -> Result<PlanKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "income" | "收入" => Ok(PlanKind::Income),
        "expense" | "开销" | "支出" => Ok(PlanKind::Expense),
        other => bail!("计划类型只认 income（收入）或 expense（开销），收到：{other}"),
    }
}

/// `--limit` 的取值：不传 = 不改；`-` / `none` / `不设` = 显式取消额度。
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
        bail!("额度要大于 0；不想要额度就传 --limit -");
    }
    Ok(Some(amount))
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

    // 先收入后支出：收入是「这个月有多少钱进来」，开销是「这些钱怎么分」，看表顺序也跟着来
    let income: Vec<&PlanRow> = rows
        .iter()
        .filter(|row| row.plan.kind == PlanKind::Income)
        .collect();
    let expense: Vec<&PlanRow> = rows
        .iter()
        .filter(|row| row.plan.kind == PlanKind::Expense)
        .collect();
    print_section("收入计划", &income, PlanKind::Income, name_width);
    print_section("开销计划", &expense, PlanKind::Expense, name_width);
}

/// 一种类型一段：各自的表头、各自的合计（收入看「目标 / 已入 / 差距」，开销看「上限 / 已花 / 剩余」）。
fn print_section(title: &str, rows: &[&PlanRow], kind: PlanKind, name_width: usize) {
    let income = kind == PlanKind::Income;
    let (limit_head, spent_head, left_head) = if income {
        ("目标", "已入", "差距")
    } else {
        ("上限", "已花", "剩余")
    };

    println!();
    println!("  {title}");
    if rows.is_empty() {
        println!("    （还没有{title} —— 计划页按 a 新建，把类型选成{}）", if income { "收入" } else { "开销" });
        return;
    }

    println!(
        "  {}  {}  {}  {}",
        pad_right("名称", name_width),
        pad_left(limit_head, MONEY_WIDTH),
        pad_left(spent_head, MONEY_WIDTH),
        pad_left(left_head, MONEY_WIDTH)
    );
    print_rule(name_width);

    for row in rows {
        let limit_text = match row.plan.limit {
            Some(limit) => money::format_amount(limit),
            None if income => "不设".to_string(),
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
    let capped: Vec<&&PlanRow> = rows.iter().filter(|row| row.plan.has_limit()).collect();
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
        if income {
            println!("（不设 = 不设目标，只累计入账）");
        } else {
            println!("（不限 = 不设上限，只累计花掉多少，不判超支）");
        }
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
        "{} 收入 {} · 开销额度 {}",
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
    fn kind_accepts_english_and_chinese() {
        assert_eq!(kind_of("income").expect("英文收入"), PlanKind::Income);
        assert_eq!(kind_of("收入").expect("中文收入"), PlanKind::Income);
        assert_eq!(kind_of("EXPENSE").expect("英文开销"), PlanKind::Expense);
        assert_eq!(kind_of(" 开销 ").expect("中文开销"), PlanKind::Expense);
        assert!(kind_of("工资").is_err(), "类型只认那四个词");
    }

    #[test]
    fn receipt_speaks_the_language_of_each_kind() {
        let book = |name: &str| crate::bill_book::model::BillBook {
            id: 1,
            name: name.to_string(),
        };
        let expense = crate::bill_book::model::Plan::new(
            book("房租"),
            Some(3000_00),
            PlanKind::Expense,
        );
        let income = crate::bill_book::model::Plan::new(
            book("工资"),
            Some(8000_00),
            PlanKind::Income,
        );
        assert_eq!(expense.describe_limit(), "每月 3,000.00 元");
        assert_eq!(income.describe_limit(), "每月目标 8,000.00 元");
    }
}
