//! Biller —— 账单记账微服务。
//!
//! 分层约定：每个业务领域一个文件夹，内部固定为
//! `model`（数据结构）→ `dao`（SQL）→ `service`（业务规则）→ `tui`（终端交互）。
//! 领域之间只允许通过对方的 `service` 交互，公共能力放在 [`common`]。

pub mod bill;
pub mod bill_book;
pub mod category;
pub mod common;
pub mod month;
pub mod plan;
pub mod tui;
