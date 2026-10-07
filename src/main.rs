//! 命令行入口：只做参数解析、服务装配与分发，业务逻辑全部在各自的 service 里。

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};

use biller::bill::service::BillService;
use biller::bill::tui::{self as bill_tui, BillCmd};
use biller::bill_book::service::BookService;
use biller::bill_book::tui::{self as book_tui, BookCmd};
use biller::category::service::CategoryService;
use biller::category::tui::{self as category_tui, CategoryCmd};
use biller::common::{db, time};
use biller::month::service::MonthService;
use biller::month::tui::{self as month_tui, MonthCmd};
use biller::tui::{self as interactive, Services};

#[derive(Parser)]
#[command(
    name = "biller",
    version,
    about = "账单记账（PostgreSQL）；不带子命令时进入交互式界面"
)]
struct Cli {
    /// PostgreSQL 连接串，例如 postgres://user:pass@127.0.0.1:5432/biller
    ///
    /// 缺省时依次读取环境变量 DATABASE_URL、项目根目录的 .env，最后回落到内置默认值。
    #[arg(long, global = true, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// 不带子命令时默认进入交互式界面
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 打开交互式全屏界面（不带子命令时的默认行为）
    Ui,
    /// 建库 + 建表（幂等，可重复执行）
    Init,
    /// 账本管理
    Book {
        #[command(subcommand)]
        cmd: BookCmd,
    },
    /// 分类管理
    Category {
        #[command(subcommand)]
        cmd: CategoryCmd,
    },
    /// 账单管理
    Bill {
        #[command(subcommand)]
        cmd: BillCmd,
    },
    /// 月度记账
    Month {
        #[command(subcommand)]
        cmd: MonthCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // .env 只在存在时加载，不覆盖已有的环境变量。
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    let url = db::resolve_url(cli.database_url)?;
    let command = cli.command.unwrap_or(Command::Ui);

    if matches!(command, Command::Init) {
        db::ensure_database(&url).await?;
        let pool = db::connect(&url).await?;
        db::init_schema(&pool).await?;
        println!("表结构已就绪：{}", db::redact(&url));
        return Ok(());
    }

    let pool = db::connect(&url).await?;
    // 所有命令都顺手做一次幂等建表：既保证交互界面可用，
    // 也让旧库自动补建 app_state 这类后加的表。
    db::init_schema(&pool).await?;

    match command {
        Command::Init => unreachable!("init 已在上面提前返回"),
        Command::Ui => {
            let month = time::first_day_of_month(Utc::now().date_naive());
            interactive::run(Services::new(pool), month).await
        }
        Command::Book { cmd } => book_tui::run(&BookService::new(pool), cmd).await,
        Command::Category { cmd } => category_tui::run(&CategoryService::new(pool), cmd).await,
        Command::Bill { cmd } => bill_tui::run(&BillService::new(pool), cmd).await,
        Command::Month { cmd } => month_tui::run(&MonthService::new(pool), cmd).await,
    }
}
