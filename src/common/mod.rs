//! 最小化公共服务层：只放各领域都要用、且不属于任何单一领域的能力。
//!
//! - [`types`]：跨模块共享的类型别名
//! - [`money`]：金额（分 ↔ 元）
//! - [`time`]：月份 / 时间解析与格式化
//! - [`table`]：终端表格渲染
//! - [`db`]：连接池、建库、建表、错误映射
//! - [`state`]：应用级键值状态（跨会话保留）
//! - [`migrate`]：整库数据迁移（源库 → 目标库）

pub mod db;
pub mod migrate;
pub mod money;
pub mod state;
pub mod table;
pub mod time;
pub mod types;
