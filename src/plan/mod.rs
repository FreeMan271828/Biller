//! 计划领域：`model` 定规则 → `service` 对外 → `tui` 命令行。
//!
//! 计划本体就是账本（`bill_books.plan_limit`），所以这里没有自己的 `dao`：
//! 持久化归 `bill_book` 领域，账单汇总归 `bill` 领域，本领域只做组合。

pub mod model;
pub mod service;
pub mod tui;
