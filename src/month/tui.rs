//! 月度记账的终端交互层：只做参数解析与输出格式化。

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use clap::Subcommand;

use crate::common::{money, table, time};
use crate::month::service::{MonthReport, MonthService};

#[derive(Subcommand, Debug)]
pub enum MonthCmd {
    /// 设置某月起始金额
    SetStart {
        /// 月份，格式 YYYY-MM
        month: String,
        /// 起始金额（元）
        amount: String,
    },
    /// 查看某月报表
    Show {
        /// 月份，格式 YYYY-MM；缺省为当前月
        #[arg(long)]
        month: Option<String>,
    },
}

pub async fn run(service: &MonthService, command: MonthCmd) -> Result<()> {
    match command {
        MonthCmd::SetStart { month, amount } => {
            let month = time::parse_month(&month)?;
            let amount = money::parse_amount(&amount)?;
            service.set_start_balance(month, amount).await?;
            println!(
                "{} 的起始金额已设为 {} 元",
                time::format_month(month),
                money::format_amount(amount)
            );
        }
        MonthCmd::Show { month } => {
            let month = match month.as_deref() {
                Some(value) => time::parse_month(value)?,
                None => time::first_day_of_month(Utc::now().date_naive()),
            };
            let report = service.report(month).await?;
            print_report(&report);
        }
    }
    Ok(())
}

fn print_report(report: &MonthReport) {
    let collection = &report.collection;

    println!("═══ {} 月度记账 ═══", time::format_month(collection.month));
    println!("起始金额   {}", money::format_amount(collection.start_balance));
    println!("收入合计   {}", money::format_amount(collection.total_income));
    println!("支出合计   {}", money::format_amount(collection.total_expense));
    println!("本月净额   {}", money::format_amount(collection.net()));
    println!("月末结余   {}", money::format_amount(collection.remaining()));

    print_group("按账本 · 收入", &collection.income_by_book, &report.book_names);
    print_group("按账本 · 支出", &collection.expense_by_book, &report.book_names);
    print_group(
        "按分类 · 收入",
        &collection.income_by_category,
        &report.category_names,
    );
    print_group(
        "按分类 · 支出",
        &collection.expense_by_category,
        &report.category_names,
    );
}

fn print_group(title: &str, values: &HashMap<u64, i64>, names: &HashMap<u64, String>) {
    if values.is_empty() {
        return;
    }

    let mut entries: Vec<(u64, i64)> = values
        .iter()
        .map(|(id, amount)| (*id, *amount))
        .collect();
    entries.sort_by(|left, right| right.1.cmp(&left.1));

    let rows = entries
        .iter()
        .map(|(id, amount)| {
            vec![
                names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("(已删除 id={id})")),
                money::format_amount(*amount),
            ]
        })
        .collect::<Vec<_>>();

    println!("\n{title}");
    table::print_table(&["名称", "金额"], &rows);
}
