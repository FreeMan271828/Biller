//! 账单领域：一笔具体的收入/支出记录。
//!
//! 层级：`model`（数据结构）→ [`dao`]（SQL）→ [`service`]（业务规则）→ [`tui`]（终端交互）。

pub mod dao;
pub mod model;
pub mod service;
pub mod tui;

pub use model::{BillEntity, BillKind};

// 主键与金额别名统一放在 common::types，这里只是方便本领域内部使用。
pub use crate::common::types::{AmountType, BillId};
