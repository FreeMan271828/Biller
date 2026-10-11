//! 交互式 TUI（全屏页面）。
//!
//! 拆分方式：
//! - [`app`]：状态机（纯逻辑，可脱离终端测试）
//! - [`input`]：与终端库无关的按键抽象
//! - [`event`]：crossterm 事件 → [`input::Key`]
//! - [`view`]：ratatui 渲染

pub mod app;
pub mod event;
pub mod input;
pub mod view;

use std::time::Duration;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event as ct;
use ratatui::crossterm::event::Event;
use crate::common::db::Pool;

use crate::bill::service::BillService;
use crate::bill_book::service::BookService;
use crate::category::service::CategoryService;
use crate::common::{config, db, migrate};
use crate::month::service::MonthService;
use crate::plan::service::PlanService;
use crate::tui::app::{Action, App, ConnectionTarget};

/// TUI 需要的各领域 service，由 main 装配一次后共享。
#[derive(Clone)]
pub struct Services {
    pub bills: BillService,
    pub books: BookService,
    pub categories: CategoryService,
    pub months: MonthService,
    pub plans: PlanService,
    /// 当前连接池：数据迁移这类跨领域操作需要直接读源库。
    pool: Pool,
}

impl Services {
    pub fn new(pool: Pool) -> Self {
        Self {
            bills: BillService::new(pool.clone()),
            books: BookService::new(pool.clone()),
            categories: CategoryService::new(pool.clone()),
            months: MonthService::new(pool.clone()),
            plans: PlanService::new(pool.clone()),
            pool,
        }
    }

    /// 当前连接池。
    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}

/// 打开交互式界面，直到用户按 `q` 退出。
///
/// 终端初始化失败（例如输出被重定向、不是真实终端）时返回可读错误，
/// 而不是把终端留在 raw 模式。
pub async fn run(
    services: Option<Services>,
    month: NaiveDate,
    connection_url: String,
    sqlite_path: String,
) -> Result<()> {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => bail!(
            "无法初始化终端：{error}\n请在真实终端中运行（不要重定向输出或通过管道调用）"
        ),
    };

    let result = async {
        let mut app = App::new(month, &connection_url, &sqlite_path);
        match services.as_ref() {
            Some(services) => app.reload(services).await?,
            // 连不上也照样进界面：先让用户看到引导页，去设置里配好库
            None => app.mark_needs_setup(),
        }
        event_loop(&mut terminal, &mut app, services, &connection_url).await
    }
    .await;

    // 无论正常退出还是出错，都要恢复终端（光标、raw 模式、备用屏幕）。
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mut services: Option<Services>,
    connection_url: &str,
) -> Result<()> {
    let mut dirty = true;

    while !app.should_quit {
        // 只在状态变化后重绘：没有输入时不重复刷屏，避免闪烁与空转
        if dirty {
            terminal.draw(|frame| view::render(frame, app))?;
            dirty = false;
        }

        // 带超时的轮询，便于将来插入后台刷新
        if !ct::poll(Duration::from_millis(250))? {
            continue;
        }

        if let Event::Key(key) = ct::read()? {
            if let Some(mapped) = event::translate(key) {
                let action = app.on_key(mapped);
                apply_action(action, app, &mut services, connection_url).await;
                dirty = true;
            }
        }
    }
    Ok(())
}

/// 需要重建连接池的动作由外壳处理（`App` 保持纯逻辑），其余交给 [`App::apply`]。
async fn apply_action(
    action: Action,
    app: &mut App,
    services: &mut Option<Services>,
    connection_url: &str,
) {
    match action {
        Action::SaveConnection(target) => {
            // 先把设置落地：即使这次连不上，下次启动也会用这套参数。
            // 本地模式写进 config.toml，远程模式沿用原来的 .env（兼容已有习惯）。
            let (new_url, saved) = match &target {
                ConnectionTarget::Local { path } => {
                    let path = std::path::PathBuf::from(path);
                    let mut config = config::AppConfig::load().unwrap_or_default();
                    config.mode = config::DbMode::Local;
                    config.sqlite_path = path.clone();
                    let saved = config.save().map(|()| config::AppConfig::path());
                    (db::sqlite_url(&path), saved)
                }
                ConnectionTarget::Remote(settings) => {
                    let url = settings.to_url();
                    let saved = db::save_database_url(&url);

                    let mut config = config::AppConfig::load().unwrap_or_default();
                    config.mode = config::DbMode::Postgres;
                    config.postgres_url = url.clone();
                    let _ = config.save();

                    (url, saved)
                }
            };

            match initialize_database(&new_url).await {
                Ok((outcome, pool)) => {
                    *services = Some(Services::new(pool));
                    app.connection_url = new_url;

                    match &target {
                        ConnectionTarget::Local { path } => app.sqlite_path = path.clone(),
                        ConnectionTarget::Remote(settings) => app.connection = settings.clone(),
                    }

                    let mut message = match &saved {
                        Ok(path) => format!("设置已保存到 {}", path.display()),
                        Err(error) => format!("连接已切换，但写配置失败：{error:#}"),
                    };
                    match outcome {
                        db::DatabaseOutcome::Created => message.push_str("；并已建库建表"),
                        db::DatabaseOutcome::Existed => message.push_str("；表结构已就绪"),
                    }
                    app.set_info(message);

                    if let Some(services) = services.as_ref() {
                        if let Err(error) = app.reload(services).await {
                            app.set_error(format!("{error:#}"));
                        }
                    }
                }
                Err(error) => {
                    app.set_error(format!("设置已保存，但连接失败（旧连接继续可用）：{error:#}"));
                }
            }
        }
        Action::InitDatabase => match initialize_database(connection_url).await {
            Ok((outcome, pool)) => {
                pool.close().await;

                let target = match db::DbKind::from_url(connection_url) {
                    db::DbKind::Sqlite => db::sqlite_file_path(connection_url)
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                    db::DbKind::Postgres => db::database_name(connection_url).unwrap_or_default(),
                };
                let message = match outcome {
                    db::DatabaseOutcome::Created => format!("已创建 {target} 并建好表结构"),
                    db::DatabaseOutcome::Existed => format!("{target} 已就绪，表结构已同步"),
                };
                app.set_info(message);

                if let Some(services) = services.as_ref() {
                    if let Err(error) = app.reload(services).await {
                        app.set_error(format!("{error:#}"));
                    }
                }
            }
            Err(error) => app.set_error(format!("{error:#}")),
        },
        Action::MigrateData(settings) => {
            // 迁移要有源库：还没连上就先让用户把连接配好
            let Some(source) = services.as_ref().map(|services| services.pool().clone()) else {
                app.set_error("还没有连上数据库，先在设置里配置好再迁移");
                return;
            };

            let target_url = settings.to_url();

            let migration = async {
                let (_, target) = initialize_database(&target_url).await?;
                let report = migrate::migrate_all(&source, &target).await?;
                Ok::<_, anyhow::Error>((target, report))
            }
            .await;

            match migration {
                Ok((target, report)) => {
                    *services = Some(Services::new(target));
                    // 迁移完成后目标库就是新的家，顺手把设置也落地并切过去
                    let _ = db::save_database_url(&target_url);
                    app.connection = settings;
                    app.connection_url = target_url.clone();

                    app.set_info(format!(
                        "已迁移到 {}：账本 {}、分类 {}、账单 {}、月度 {}、设置 {}（共 {} 行）",
                        db::redact(&target_url),
                        report.books,
                        report.categories,
                        report.bills,
                        report.months,
                        report.settings,
                        report.total()
                    ));

                    if let Some(services) = services.as_ref() {
                        if let Err(error) = app.reload(services).await {
                            app.set_error(format!("{error:#}"));
                        }
                    }
                }
                Err(error) => {
                    app.set_error(format!("迁移失败（源库未改动，目标库已回滚）：{error:#}"));
                }
            }
        }
        // 还没连上库时，其余动作一律忽略：界面正显示引导页，用户该去设置里配库
        other => {
            if let Some(services) = services.as_ref() {
                app.apply(other, services).await;
            }
        }
    }
}

/// 建库（不存在时创建）+ 幂等建表，返回可直接使用的连接池。
///
/// 表结构语句本身幂等，所以「初始化」与「迁移」是同一个动作：
/// 后加的表/索引会在启动或点一次初始化时自动补齐。
async fn initialize_database(url: &str) -> Result<(db::DatabaseOutcome, Pool)> {
    // 先直接连目标库：能连上就只补表结构，不必去碰维护库
    match db::connect(url).await {
        Ok(pool) => {
            db::init_schema(&pool).await?;
            Ok((db::DatabaseOutcome::Existed, pool))
        }
        Err(connect_error) => {
            // 连不上（多半是库还不存在）再建库后重试；失败时抛出原始连接错误更好定位
            let outcome = db::ensure_database(url)
                .await
                .map_err(|_| connect_error)?;
            let pool = db::connect(url).await?;
            db::init_schema(&pool).await?;
            Ok((outcome, pool))
        }
    }
}
