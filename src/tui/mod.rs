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
use sqlx::PgPool;

use crate::bill::service::BillService;
use crate::bill_book::service::BookService;
use crate::category::service::CategoryService;
use crate::month::service::MonthService;
use crate::tui::app::App;

/// TUI 需要的各领域 service，由 main 装配一次后共享。
#[derive(Clone)]
pub struct Services {
    pub bills: BillService,
    pub books: BookService,
    pub categories: CategoryService,
    pub months: MonthService,
}

impl Services {
    pub fn new(pool: PgPool) -> Self {
        Self {
            bills: BillService::new(pool.clone()),
            books: BookService::new(pool.clone()),
            categories: CategoryService::new(pool.clone()),
            months: MonthService::new(pool),
        }
    }
}

/// 打开交互式界面，直到用户按 `q` 退出。
///
/// 终端初始化失败（例如输出被重定向、不是真实终端）时返回可读错误，
/// 而不是把终端留在 raw 模式。
pub async fn run(services: Services, month: NaiveDate) -> Result<()> {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => bail!(
            "无法初始化终端：{error}\n请在真实终端中运行（不要重定向输出或通过管道调用）"
        ),
    };

    let result = async {
        let mut app = App::new(month);
        app.reload(&services).await?;
        event_loop(&mut terminal, &mut app, &services).await
    }
    .await;

    // 无论正常退出还是出错，都要恢复终端（光标、raw 模式、备用屏幕）。
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    services: &Services,
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
                app.apply(action, services).await;
                dirty = true;
            }
        }
    }
    Ok(())
}
