//! 账单的终端交互层：只做参数解析与输出格式化。

use anyhow::{Result, anyhow};
use clap::Subcommand;

use crate::bill::model::{BillEntity, BillKind};
use crate::bill::service::{BillQuery, BillService, NewBill};
use crate::common::types::AmountType;
use crate::common::{money, table, time};

#[derive(Subcommand, Debug)]
pub enum BillCmd {
    /// 记一笔账（账本/分类不存在会自动创建）
    Add {
        /// 方向：income / expense（也接受 收入 / 支出）
        kind: String,
        /// 金额（元），例如 12.34
        #[arg(allow_hyphen_values = true)]
        amount: String,
        /// 账本名称
        #[arg(long)]
        book: String,
        /// 分类名称
        #[arg(long)]
        category: String,
        /// 发生时间，默认当前时间；支持 2026-10-05 或 "2026-10-05 13:20:00"
        #[arg(long)]
        date: Option<String>,
        /// 备注
        #[arg(long)]
        remark: Option<String>,
    },
    /// 修改一笔账单（未给出的字段保持原值）
    Edit {
        /// 账单 id
        id: u64,
        /// 方向：income / expense
        #[arg(long)]
        kind: Option<String>,
        /// 金额（元）
        #[arg(long, allow_hyphen_values = true)]
        amount: Option<String>,
        /// 账本名称
        #[arg(long)]
        book: Option<String>,
        /// 分类名称
        #[arg(long)]
        category: Option<String>,
        /// 发生时间
        #[arg(long)]
        date: Option<String>,
        /// 备注
        #[arg(long)]
        remark: Option<String>,
    },
    /// 查询账单
    List {
        /// 月份，格式 YYYY-MM
        #[arg(long)]
        month: Option<String>,
        /// 按账本名称筛选
        #[arg(long)]
        book: Option<String>,
        /// 按分类名称筛选
        #[arg(long)]
        category: Option<String>,
        /// 按方向筛选：income / expense
        #[arg(long)]
        kind: Option<String>,
        /// 最多返回多少条
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// 删除账单
    Rm {
        /// 账单 id
        id: u64,
    },
}

pub async fn run(service: &BillService, command: BillCmd) -> Result<()> {
    match command {
        BillCmd::Add {
            kind,
            amount,
            book,
            category,
            date,
            remark,
        } => {
            let kind = parse_kind(&kind)?;
            let amount = money::parse_amount(&amount)?;
            let created_at = time::parse_datetime(date.as_deref())?;

            let (bill, auto_created) = service
                .add(NewBill {
                    kind,
                    amount,
                    book,
                    category,
                    created_at,
                    remark,
                })
                .await?;

            for item in &auto_created {
                println!("已自动创建{item}");
            }
            print_bill(&bill);
        }
        BillCmd::Edit {
            id,
            kind,
            amount,
            book,
            category,
            date,
            remark,
        } => {
            // 先读原值，未指定的字段沿用
            let current = service.get(id).await?;

            let kind = match kind.as_deref() {
                Some(value) => parse_kind(value)?,
                None => current.kind,
            };
            let amount = match amount.as_deref() {
                Some(value) => money::parse_amount(value)?,
                None => current.amount,
            };
            let created_at = match date.as_deref() {
                Some(value) => time::parse_datetime(Some(value))?,
                None => current.created_at,
            };
            let remark = remark.or_else(|| current.remark.clone());

            let updated = service
                .update(
                    id,
                    NewBill {
                        kind,
                        amount,
                        book: book.unwrap_or_else(|| current.book.name.clone()),
                        category: category.unwrap_or_else(|| current.category.name.clone()),
                        created_at,
                        remark,
                    },
                )
                .await?;

            println!("已更新账单 #{}", updated.id);
            print_bill(&updated);
        }
        BillCmd::List {
            month,
            book,
            category,
            kind,
            limit,
        } => {
            let query = BillQuery {
                month: month.as_deref().map(time::parse_month).transpose()?,
                book,
                category,
                kind: kind.as_deref().map(parse_kind).transpose()?,
                limit: Some(limit),
            };

            let bills = service.list(query).await?;
            let rows = bills
                .iter()
                .map(|bill| {
                    vec![
                        bill.id.to_string(),
                        time::format_date(bill.created_at),
                        bill.kind.to_string(),
                        money::format_amount(bill.amount),
                        bill.book.name.clone(),
                        bill.category.name.clone(),
                        bill.remark.clone().unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>();

            table::print_table(
                &["ID", "日期", "方向", "金额", "账本", "分类", "备注"],
                &rows,
            );

            if !bills.is_empty() {
                let income: AmountType = bills
                    .iter()
                    .filter(|bill| bill.kind == BillKind::Income)
                    .map(|bill| bill.amount)
                    .sum();
                let expense: AmountType = bills
                    .iter()
                    .filter(|bill| bill.kind == BillKind::Expense)
                    .map(|bill| bill.amount)
                    .sum();
                println!(
                    "共 {} 条：收入 {} 元，支出 {} 元",
                    bills.len(),
                    money::format_amount(income),
                    money::format_amount(expense)
                );
            }
        }
        BillCmd::Rm { id } => {
            service.remove(id).await?;
            println!("已删除账单 id={id}");
        }
    }
    Ok(())
}

fn parse_kind(value: &str) -> Result<BillKind> {
    BillKind::parse(value)
        .ok_or_else(|| anyhow!("无法识别的方向「{value}」，请用 income 或 expense"))
}

fn print_bill(bill: &BillEntity) {
    println!(
        "  #{} {} {} 元 | 账本 {} | 分类 {} | {} | 备注 {}",
        bill.id,
        bill.kind,
        money::format_amount(bill.amount),
        bill.book.name,
        bill.category.name,
        time::format_date(bill.created_at),
        bill.remark.as_deref().unwrap_or("-")
    );
}
