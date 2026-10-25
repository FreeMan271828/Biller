//! 银行流水同步：复用本机浏览器里已登录的招行会话，拉流水 → 核对 → 入库。
//!
//! 分层与其他领域一致：
//! - [`model`]：数据结构与纯逻辑（同步窗口、返回体解析、去重指纹、店家名归一化）
//! - `client`：构造请求（请求体 + 页面内执行的 JS）
//! - `browser`：CDP 通道（启动/连接浏览器、在页面里执行 JS）
//! - `dao`：4 张表的 SQL（同步账户 / 已导入流水 / 店家记忆 / 关键词规则）
//! - `service`：业务规则（窗口计算、去重、记忆系统、导入）

pub mod browser;
pub mod capture;
pub mod capture_proxy;
pub mod client;
pub mod dao;
pub mod model;
pub mod proxy_ca;
pub mod service;
pub mod sync;
pub mod tui;
