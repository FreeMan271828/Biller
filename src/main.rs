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
use biller::common::{config, db, time};
use biller::month::service::MonthService;
use biller::month::tui::{self as month_tui, MonthCmd};
use biller::plan::service::PlanService;
use biller::plan::tui::{self as plan_tui, PlanCmd};
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
    /// 开销计划（预计金额 / 攒钱金额 → 上限开销）
    Plan {
        #[command(subcommand)]
        cmd: PlanCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 设置页把连接串写进 .env；这里让它覆盖 shell 里可能存在的同名变量，
    // 这样「应用里保存的设置」才是最终生效的那个（显式的 --database-url 仍然最优先）。
    let _ = dotenvy::from_path_override(std::path::Path::new(".env"));

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Ui);

    // 配置文件决定「用哪个库」；默认是本地 SQLite 文件，零配置即可跑起来
    let settings = config::AppConfig::load()?;
    let url = settings.resolve_url(cli.database_url);

    if matches!(command, Command::Init) {
        let outcome = db::ensure_database(&url).await?;
        let pool = db::connect(&url).await?;
        db::init_schema(&pool).await?;

        match db::DbKind::from_url(&url) {
            // 本地模式没有「库」这个概念，报文件路径更有用
            db::DbKind::Sqlite => match db::sqlite_file_path(&url) {
                Some(path) => println!("本地数据库已就绪：{}", path.display()),
                None => println!("本地数据库已就绪"),
            },
            db::DbKind::Postgres => {
                let database = db::database_name(&url)?;
                match outcome {
                    db::DatabaseOutcome::Created => println!("已创建数据库 {database}"),
                    db::DatabaseOutcome::Existed => println!("数据库 {database} 已存在"),
                }
            }
        }

        println!("表结构已就绪：{}", db::redact(&url));
        return Ok(());
    }

    // 连不上也照样进界面：把 None 交给 TUI，让它显示配置引导页。
    // 只有非交互子命令才必须真的连上。
    let (pool, services) = match db::connect(&url).await {
        Ok(pool) => {
            // 所有命令都顺手做一次幂等建表：既保证交互界面可用，
            // 也让旧库自动补建 app_state 这类后加的表。
            db::init_schema(&pool).await?;
            (Some(pool.clone()), Some(Services::new(pool)))
        }
        Err(error) => {
            eprintln!("⚠ 暂时连不上数据库：{error:#}");
            eprintln!("  已经进入界面，请在「设置 → 数据库连接」里配置。");
            (None, None)
        }
    };

    match command {
        Command::Init => unreachable!("init 已在上面提前返回"),
        Command::Ui => {
            let month = time::first_day_of_month(Utc::now().date_naive());
            let sqlite_path = settings.sqlite_path.to_string_lossy().to_string();
            interactive::run(services, month, url, sqlite_path).await
        }
        Command::Book { cmd } => {
            let pool = require_pool(&pool)?;
            book_tui::run(&BookService::new(pool), cmd).await
        }
        Command::Category { cmd } => {
            let pool = require_pool(&pool)?;
            category_tui::run(&CategoryService::new(pool), cmd).await
        }
        Command::Bill { cmd } => {
            let pool = require_pool(&pool)?;
            bill_tui::run(&BillService::new(pool), cmd).await
        }
        Command::Month { cmd } => {
            let pool = require_pool(&pool)?;
            month_tui::run(&MonthService::new(pool), cmd).await
        }
        Command::Plan { cmd } => {
            let pool = require_pool(&pool)?;
            plan_tui::run(&PlanService::new(pool), cmd).await
        }
    }
}

/// 非交互子命令必须要有一个能用的连接池。
fn require_pool(pool: &Option<db::Pool>) -> Result<db::Pool> {
    pool.clone().ok_or_else(|| {
        anyhow::anyhow!("数据库不可用：请先 `biller init`，或用 --database-url 指定连接")
    })
}
