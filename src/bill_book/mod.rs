//! 账本领域：钱的来源/去向容器（如「微信」「支付宝」）。
//!
//! 层级：`model`（数据结构）→ [`dao`]（SQL）→ [`service`]（业务规则）→ [`tui`]（终端交互）。

pub mod dao;
pub mod model;
pub mod service;
pub mod tui;

pub use model::BillBook;
