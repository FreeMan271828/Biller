//! 月度记账领域：把某月账单汇总成 [`BillMonthCollection`]。
//!
//! 层级：`model`（数据结构）→ [`dao`]（SQL）→ [`service`]（业务规则）→ [`tui`]（终端交互）。

pub mod dao;
pub mod model;
pub mod service;
pub mod tui;

pub use model::BillMonthCollection;
