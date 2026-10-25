//! 一次同步的执行流程：启动/连上浏览器 → **监听网络请求取会话信息** →
//! 用页面发查询 → 解析。
//!
//! 取信息的方式与 Proxyman 的思路一致：订阅 CDP 的 `Network.*` 事件，按
//! `https://cbsh.ebank.cmbchina.com` 前缀过滤，从**真实请求**里拿
//! `ClientNo`（请求头）和 `accountUid`/`subAccountNo`（请求体）。
//! 不用装证书、也不改系统代理——只监听那一个标签页。

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::bank::browser::{self, SniffedSession, TargetInfo};
use crate::bank::client::{self, BankAccount};
use crate::bank::model::{self, BankTransaction, SyncWindow};

/// 监听窗口：给用户留出登录/刷新的时间。
const SNIFF_TIMEOUT: Duration = Duration::from_secs(25);

/// 就绪的同步会话。
#[derive(Debug, Clone)]
pub struct Session {
    /// 监听到的会话信息（ClientNo / Cookie / 账号）。
    pub sniffed: SniffedSession,
    /// 招行标签页的地址。
    pub tab_url: String,
    web_socket_url: String,
}

impl Session {
    /// 从监听结果里得到可直接记账用的账户。
    pub fn account(&self) -> Option<BankAccount> {
        self.sniffed.account()
    }

    /// 最终使用的 ClientNo：优先监听到的。
    pub fn client_no(&self) -> Option<&str> {
        self.sniffed.client_no.as_deref()
    }
}

/// 准备好浏览器并返回招行标签页的 (WebSocket 地址, 页面 URL)。
///
/// 端口没开就自己启动（独立配置目录 + 免代理，避免继承 Proxyman 导致 403）。
pub async fn bank_target(port: u16) -> Result<(String, String)> {
    let targets = match browser::probe(port).await {
        Ok(targets) => targets,
        Err(_) => {
            browser::launch(port, client::ORIGIN)
                .context("启动浏览器失败，请确认装了 Chrome 或 Edge")?;
            browser::wait_until_ready(port).await?
        }
    };

    let target: &TargetInfo = browser::pick_bank_target(&targets).context(
        "浏览器里没有招商银行的页面。\n\
         请在那个窗口里打开 https://cbsh.ebank.cmbchina.com 并登录",
    )?;

    Ok((target.web_socket_url.clone(), target.url.clone()))
}

/// 刷新页面 → 等它自己发请求 → 用新连接被动监听。
///
/// 必须分两步：`Page.reload` 会把调试 WebSocket 重置掉，同一个连接收不到后续事件。
async fn sniff_after_reload(web_socket_url: &str) -> Result<SniffedSession> {
    browser::reload_page(web_socket_url).await;
    // 给页面重新发请求留点时间
    tokio::time::sleep(Duration::from_millis(1500)).await;
    browser::sniff_session(web_socket_url, SNIFF_TIMEOUT).await
}

/// 一次探测的结果（给 `biller sync probe` 用）。
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub port: u16,
    pub tabs: usize,
    pub bank_tab: Option<String>,
    pub sniffed: SniffedSession,
    /// 这次探测是不是顺手启动了一个浏览器窗口。
    pub launched: bool,
}

/// 探测：连上调试端口（没开就顺手启动一个）→ 找到招行页面 → 监听一轮网络事件。
pub async fn probe(port: u16) -> Result<ProbeReport> {
    let mut launched = false;

    let targets = match browser::probe(port).await {
        Ok(targets) => targets,
        Err(_) => {
            browser::launch(port, client::ORIGIN)
                .context("自动启动浏览器失败，请确认装了 Chrome 或 Edge")?;
            launched = true;
            browser::wait_until_ready(port).await?
        }
    };

    let bank_tab = browser::pick_bank_target(&targets);
    let sniffed = match bank_tab {
        Some(target) => sniff_after_reload(&target.web_socket_url).await?,
        None => SniffedSession::default(),
    };

    Ok(ProbeReport {
        port,
        tabs: targets.len(),
        bank_tab: bank_tab.map(|target| target.url.clone()),
        sniffed,
        launched,
    })
}

/// 确保浏览器 + 招行页面就绪，并监听出会话信息；必要时自己拉起浏览器。
pub async fn ensure_session(port: u16) -> Result<Session> {
    let targets = match browser::probe(port).await {
        Ok(targets) => targets,
        Err(_) => {
            // 没有可用的调试端口：用独立配置目录拉起一个实例（不影响日常浏览器）
            browser::launch(port, client::ORIGIN)
                .context("启动浏览器失败，请确认装了 Chrome 或 Edge")?;
            browser::wait_until_ready(port).await?
        }
    };

    let target: &TargetInfo = browser::pick_bank_target(&targets).context(
        "浏览器里没有招商银行的页面。\n\
         请在那个窗口里打开 https://cbsh.ebank.cmbchina.com 并登录后再试",
    )?;

    // 监听（会顺带刷新一次页面，让网银自己重发请求）
    let mut sniffed = sniff_after_reload(&target.web_socket_url).await?;

    // ClientNo 没抓到的话，退回扫页面存储（老版本页面可能没有这个头）
    if sniffed.client_no.is_none() {
        sniffed.client_no = browser::find_client_no(&target.web_socket_url).await?;
    }

    if sniffed.client_no.is_none() {
        bail!(
            "没监听到 ClientNo。请确认那个窗口里已经登录招商银行网银，\
             并且刚才有接口请求（页面会自动刷新一次）。\n捕获情况：{}",
            sniffed.describe()
        );
    }

    Ok(Session {
        sniffed,
        tab_url: target.url.clone(),
        web_socket_url: target.web_socket_url.clone(),
    })
}

/// 用已有会话拉取指定窗口的流水。
pub async fn fetch_with(
    session: &Session,
    account: &BankAccount,
    window: &SyncWindow,
) -> Result<Vec<BankTransaction>> {
    let client_no = session
        .client_no()
        .context("会话里没有 ClientNo，请重新跑一次同步")?;

    let script = client::querytrans_js(account, window, client_no);
    let raw = browser::evaluate(&session.web_socket_url, &script)
        .await
        .context("在页面里执行查询失败")?;

    let text = client::unwrap_js_result(&raw)?;
    model::parse_querytrans_response(&text)
}

/// 一步到位：确保会话 → 拉取。
///
/// 账户优先用**监听到的**（最准），监听不到才用数据库里保存的兜底。
pub async fn fetch(
    port: u16,
    fallback_account: Option<&BankAccount>,
    window: &SyncWindow,
) -> Result<Vec<BankTransaction>> {
    let session = ensure_session(port).await?;
    let account = session
        .account()
        .or_else(|| fallback_account.cloned())
        .context(
            "没从页面请求里读到 accountUid / subAccountNo。\n\
             请确认在那个窗口里已登录招商银行网银并刷新一次，\
             或用 `biller sync account add` 兜底保存账户",
        )?;

    fetch_with(&session, &account, window).await
}

/// 数据库里保存过的默认账户；没有就返回 `None`（正常流程不依赖它）。
pub async fn stored_account(
    service: &crate::bank::service::BankService,
) -> Result<Option<BankAccount>> {
    let stored = service.stored_accounts().await?;
    Ok(stored
        .into_iter()
        .find(|item| item.is_default)
        .map(|item| item.account))
}

/// 校验调试端口参数。
pub fn ensure_port(port: u16) -> Result<u16> {
    if port == 0 {
        bail!("调试端口不能为 0");
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_port() {
        assert!(ensure_port(0).is_err());
        assert_eq!(ensure_port(9222).unwrap(), 9222);
    }

    #[test]
    fn session_prefers_the_sniffed_account() {
        let session = Session {
            sniffed: SniffedSession {
                client_no: Some("c".to_string()),
                cookie: None,
                account_uid: Some("P-sniffed".to_string()),
                sub_account_no: Some("111".to_string()),
                captured_requests: 2,
            },
            tab_url: "https://cbsh.ebank.cmbchina.com/x".to_string(),
            web_socket_url: "ws://x".to_string(),
        };

        let account = session.account().expect("应能从监听结果拿到账户");
        assert_eq!(account.account_uid, "P-sniffed");
        assert_eq!(session.client_no(), Some("c"));
    }

    #[test]
    fn session_without_account_falls_back() {
        let session = Session {
            sniffed: SniffedSession::default(),
            tab_url: String::new(),
            web_socket_url: String::new(),
        };

        assert!(session.account().is_none());
        assert!(session.client_no().is_none());
    }
}
