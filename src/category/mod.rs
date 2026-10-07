//! 分类领域：账单的归属分类，支持父子层级。
//!
//! 层级：`model`（数据结构）→ [`dao`]（SQL）→ [`service`]（业务规则）→ [`tui`]（终端交互）。

pub mod dao;
pub mod model;
pub mod service;
pub mod tui;

pub use model::Category;
