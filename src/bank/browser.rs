//! 通过 Chrome DevTools 协议（CDP）复用浏览器里已登录的招行会话。
//!
//! 为什么这么做：`Cookie` 和 `ClientNo` 都在浏览器的登录会话里，Cookie 通常
//! 几十分钟就过期。与其自己去解密浏览器的 cookie 库（Chrome 的加密方案还常变），
//! 不如直接连上浏览器，**让它自己去发请求**——Cookie、Origin、`Sec-Fetch-*`
//! 全由浏览器补上，我们只负责把 body 和业务请求头写对。
//!
//! 注意：Chrome 136+ 不允许在**默认配置目录**上开调试端口，所以这里用独立目录
//! `%USERPROFILE%\.biller\browser` 启动一个实例，你在那个窗口里登录一次即可。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::bank::client::{ORIGIN, looks_like_client_no};

/// 默认调试端口。
pub const DEFAULT_PORT: u16 = 9222;

/// CDP 里用的请求 id（一次连接只发一个请求，固定为 1）。
const REQUEST_ID: i64 = 1;

/// 等浏览器起来的最长时间。
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// 一个调试目标（一般就是标签页）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    pub url: String,
    /// 连这个标签页用的 WebSocket 地址。
    pub web_socket_url: String,
}

/// 浏览器实例的配置目录（独立目录，不动你日常用的 Chrome 配置）。
pub fn user_data_dir() -> PathBuf {
    crate::common::db::default_sqlite_path()
        .parent()
        .map(|parent| parent.join("browser"))
        .unwrap_or_else(|| PathBuf::from("browser"))
}

/// 可能的浏览器可执行文件位置（Windows 常见路径）。
const BROWSER_CANDIDATES: [&str; 4] = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
];

/// 找一个能用的 Chrome / Edge。
pub fn find_browser() -> Option<PathBuf> {
    BROWSER_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// 启动浏览器时带的参数。
pub fn browser_args(port: u16, user_data_dir: &Path, url: &str) -> Vec<String> {
    vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", user_data_dir.display()),
        // **必须**：否则会继承系统代理（比如 Proxyman），请求全被它接走
        // 并返回 403（实测过：Remote Address 会变成 127.0.0.1:9090）
        "--no-proxy-server".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        url.to_string(),
    ]
}

/// 用独立配置目录 + 调试端口启动浏览器。
pub fn launch(port: u16, url: &str) -> Result<Child> {
    let browser =
        find_browser().context("没找到 Chrome 或 Edge，请先安装其中一个（用于复用登录会话）")?;
    let directory = user_data_dir();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("创建浏览器配置目录失败：{}", directory.display()))?;

    Command::new(&browser)
        .args(browser_args(port, &directory, url))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动浏览器失败：{}", browser.display()))
}

/// 用独立配置目录 + **走本地捕获代理**启动浏览器（MITM 捕获用）。
///
/// 这样用户不用自己记 `--proxy-server` 参数；关掉这个窗口就恢复正常网络。
pub fn launch_through_proxy(proxy_port: u16) -> Result<Child> {
    let browser = find_browser().context("没找到 Chrome 或 Edge，请先安装其中一个")?;
    let directory = user_data_dir();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("创建浏览器配置目录失败：{}", directory.display()))?;

    Command::new(&browser)
        .arg(format!("--proxy-server=127.0.0.1:{proxy_port}"))
        .arg(format!("--user-data-dir={}", directory.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(ORIGIN)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动浏览器失败：{}", browser.display()))
}

/// 探测调试端口，返回当前所有标签页。
pub async fn probe(port: u16) -> Result<Vec<TargetInfo>> {
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .context("创建 HTTP 客户端失败")?;

    let response = client
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("连不上浏览器调试端口 {port}"))?;

    let text = response.text().await.context("读取调试端口返回失败")?;
    parse_targets(&text)
}

/// 启动后轮询，直到调试端口可用（或超时）。
pub async fn wait_until_ready(port: u16) -> Result<Vec<TargetInfo>> {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;

    loop {
        match probe(port).await {
            Ok(targets) => return Ok(targets),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error).context("等了 20 秒浏览器还没准备好调试端口");
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 解析 `/json/list` 的返回；只保留有 WebSocket 地址的标签页。
pub fn parse_targets(text: &str) -> Result<Vec<TargetInfo>> {
    let value: Value = serde_json::from_str(text).context("调试端口返回的不是 JSON")?;
    let Some(items) = value.as_array() else {
        bail!("调试端口返回的格式不认识");
    };

    Ok(items
        .iter()
        .filter_map(|item| {
            let web_socket_url = item
                .get("webSocketDebuggerUrl")
                .and_then(|value| value.as_str())?;
            let url = item
                .get("url")
                .and_then(|value| value.as_str())
                .unwrap_or("");

            Some(TargetInfo {
                url: url.to_string(),
                web_socket_url: web_socket_url.to_string(),
            })
        })
        .collect())
}

/// 从标签页里挑出货银行页面。
pub fn pick_bank_target(targets: &[TargetInfo]) -> Option<&TargetInfo> {
    targets.iter().find(|target| target.url.starts_with(ORIGIN))
}

/// 在目标页面里跑一段 JS，返回它的字符串结果。
pub async fn evaluate(web_socket_url: &str, expression: &str) -> Result<String> {
    let (mut socket, _) = tokio_tungstenite::connect_async(web_socket_url)
        .await
        .with_context(|| format!("连接浏览器调试通道失败：{web_socket_url}"))?;

    let request = json!({
        "id": REQUEST_ID,
        "method": "Runtime.evaluate",
        "params": {
            "expression": expression,
            // 页面里用了 async/await，要让 CDP 等 Promise 落地
            "awaitPromise": true,
            "returnByValue": true,
            "userGesture": true,
        }
    });

    socket
        .send(Message::Text(request.to_string().into()))
        .await
        .context("向浏览器发送脚本失败")?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let next = tokio::time::timeout_at(deadline, socket.next())
            .await
            .context("等浏览器执行脚本超时（30 秒）")?;

        let Some(message) = next else {
            bail!("浏览器调试通道被关闭了");
        };
        let message = message.context("读取浏览器响应失败")?;

        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            // 事件、心跳之类先跳过
            _ => continue,
        };

        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => continue,
        };

        // 没有 id 的是事件通知，不是我们这条请求的响应
        if value.get("id").and_then(|id| id.as_i64()) != Some(REQUEST_ID) {
            continue;
        }

        if let Some(error) = value.get("error") {
            bail!("浏览器执行脚本报错：{error}");
        }

        // 页面里抛异常
        if let Some(details) = value.pointer("/result/exceptionDetails") {
            let message = details
                .pointer("/exception/description")
                .and_then(|item| item.as_str())
                .or_else(|| details.get("text").and_then(|item| item.as_str()))
                .unwrap_or("未知异常");
            bail!("页面脚本抛错：{message}");
        }

        let result = value.pointer("/result/result").unwrap_or(&Value::Null);
        return match result.get("value") {
            Some(Value::String(text)) => Ok(text.clone()),
            Some(other) => Ok(other.to_string()),
            None => bail!("页面脚本没有返回值"),
        };
    }
}

/// 数一下该域名下的 cookie 数量（只用来在界面上显示会话是否还在）。
pub async fn cookie_count(web_socket_url: &str, domain: &str) -> Result<usize> {
    let expression = format!(
        "(async () => {{
            const cookies = await document.cookie;
            return document.cookie ? document.cookie.split(';').length : 0;
        }})()"
    );

    // 用 document.cookie 就够了：这是同源页面，能直接读
    let _ = domain;
    let raw = evaluate(web_socket_url, &expression).await?;
    Ok(raw.trim().parse::<usize>().unwrap_or(0))
}

/// 扫描 localStorage / sessionStorage 的 JS。
pub fn client_no_scan_js() -> &'static str {
    r#"(() => {
  const items = [];
  const scan = (store, where) => {
    if (!store) return;
    for (let index = 0; index < store.length; index++) {
      const key = store.key(index);
      const value = store.getItem(key);
      if (value) items.push({ where, key, value });
    }
  };
  scan(window.localStorage, "localStorage");
  scan(window.sessionStorage, "sessionStorage");
  return JSON.stringify(items);
})()"#
}

/// 在页面存储里找 `ClientNo`。
pub async fn find_client_no(web_socket_url: &str) -> Result<Option<String>> {
    let raw = evaluate(web_socket_url, client_no_scan_js()).await?;
    Ok(parse_client_no(&raw))
}

/// 从扫描结果里挑出 ClientNo：先按值的形状认，再退回按键名认。
pub fn parse_client_no(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let items = value.as_array()?;

    // 第一优先：值的形状就是 UUID+13位时间戳
    for item in items {
        if let Some(value) = item.get("value").and_then(|item| item.as_str()) {
            if looks_like_client_no(value) {
                return Some(value.trim().to_string());
            }
        }
    }

    // 第二优先：键名里有 clientno（大小写不敏感）
    for item in items {
        let key = item.get("key").and_then(|item| item.as_str()).unwrap_or("");
        let value = item
            .get("value")
            .and_then(|item| item.as_str())
            .unwrap_or("");

        if key.to_ascii_lowercase().contains("clientno") && !value.trim().is_empty() {
            return Some(value.trim().to_string());
        }
    }

    None
}

/// 从网络事件里监听出来的会话信息。
///
/// 这是「像 Proxyman 一样监听」的落地：不猜 localStorage，而是从页面真实
/// 发出的请求头/请求体里取。
#[derive(Debug, Clone, Default)]
pub struct SniffedSession {
    /// 请求头 `ClientNo`。
    pub client_no: Option<String>,
    /// 请求头 `Cookie`（只用于界面显示会话是否还在）。
    pub cookie: Option<String>,
    /// 请求体里的 `accountUid`。
    pub account_uid: Option<String>,
    /// 请求体里的 `subAccountNo`。
    pub sub_account_no: Option<String>,
    /// 这段时间里匹配到的招行请求条数。
    pub captured_requests: usize,
}

impl SniffedSession {
    /// 记账要用的三个值都拿到了。
    pub fn is_complete(&self) -> bool {
        self.client_no.is_some() && self.account_uid.is_some() && self.sub_account_no.is_some()
    }

    /// 拼成账户对象；缺一半就返回 None。
    pub fn account(&self) -> Option<crate::bank::client::BankAccount> {
        Some(crate::bank::client::BankAccount {
            account_uid: self.account_uid.clone()?,
            sub_account_no: self.sub_account_no.clone()?,
        })
    }

    pub fn describe(&self) -> String {
        format!(
            "捕获 {} 条招行请求｜ClientNo={}｜accountUid={}｜subAccountNo={}",
            self.captured_requests,
            self.client_no.as_deref().unwrap_or("未取到"),
            self.account_uid.as_deref().unwrap_or("未取到"),
            self.sub_account_no.as_deref().unwrap_or("未取到"),
        )
    }
}

/// 是不是招行的接口（Proxyman 里那个筛选条件的等价物）。
///
/// 必须落在域名边界上：`https://cbsh.ebank.cmbchina.com` 后面要么结束，
/// 要么紧跟 `/`、`?`、`#`——否则 `cbsh.ebank.cmbchina.com.evil.com` 也会被当成命中。
pub fn url_matches(url: &str) -> bool {
    match url.strip_prefix(ORIGIN) {
        Some(rest) => {
            rest.is_empty()
                || rest.starts_with('/')
                || rest.starts_with('?')
                || rest.starts_with('#')
        }
        None => false,
    }
}

/// 从 CDP 的 headers 对象里取一个头（大小写不敏感）。
pub fn header_value(headers: &Value, name: &str) -> Option<String> {
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 从请求体里挖 `accountUid` / `subAccountNo`。
///
/// 兼容两种编码：JSON（招行实际用的）和表单编码（`a=1&b=2`）。
pub fn extract_account_fields(body: &str) -> (Option<String>, Option<String>) {
    let body = body.trim();
    if body.is_empty() {
        return (None, None);
    }

    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let mut account_uid = None;
        let mut sub_account_no = None;
        dig_account_fields(&value, &mut account_uid, &mut sub_account_no);
        if account_uid.is_some() || sub_account_no.is_some() {
            return (account_uid, sub_account_no);
        }
    }

    // 表单编码兜底
    let mut account_uid = None;
    let mut sub_account_no = None;
    for pair in body.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "accountUid" => account_uid = Some(value.to_string()),
            "subAccountNo" => sub_account_no = Some(value.to_string()),
            _ => {}
        }
    }

    (account_uid, sub_account_no)
}

/// 在 JSON 里递归找那两个字段（响应体里常常嵌好几层）。
fn dig_account_fields(
    value: &Value,
    account_uid: &mut Option<String>,
    sub_account_no: &mut Option<String>,
) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                match key.as_str() {
                    "accountUid" => {
                        if let Some(text) = item.as_str() {
                            let text = text.trim();
                            if !text.is_empty() {
                                *account_uid = Some(text.to_string());
                            }
                        }
                    }
                    "subAccountNo" => {
                        if let Some(text) = item.as_str() {
                            let text = text.trim();
                            if !text.is_empty() {
                                *sub_account_no = Some(text.to_string());
                            }
                        } else if let Some(number) = item.as_i64() {
                            *sub_account_no = Some(number.to_string());
                        }
                    }
                    _ => dig_account_fields(item, account_uid, sub_account_no),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                dig_account_fields(item, account_uid, sub_account_no);
            }
        }
        _ => {}
    }
}

/// 触发一次页面刷新，让网银自己重发请求。
///
/// **刷新会让调试 WebSocket 被重置**（页面进程换了），所以这里发完命令就放弃
/// 连接、忽略错误；监听要用新连接另起一轮（见 `sync::sniff_after_reload`）。
pub async fn reload_page(web_socket_url: &str) {
    let Ok((mut socket, _)) = tokio_tungstenite::connect_async(web_socket_url).await else {
        return;
    };

    for (id, method, params) in [
        (1_i64, "Network.enable", json!({})),
        (2, "Page.enable", json!({})),
        (3, "Page.reload", json!({ "ignoreCache": false })),
    ] {
        let request = json!({ "id": id, "method": method, "params": params });
        let _ = socket.send(Message::Text(request.to_string().into())).await;
        // 给浏览器一点时间把命令吃进去，否则 reload 可能没发出去连接就断了
        let _ = tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 监听一段时间内的网络事件，从真实请求里取 ClientNo / accountUid / subAccountNo。
///
/// 只做被动监听（不刷新页面）。连接中途被重置（比如页面正好在导航）时，
/// 把已经采到的部分交出去，而不是报错。
pub async fn sniff_session(web_socket_url: &str, timeout: Duration) -> Result<SniffedSession> {
    let (mut socket, _) = tokio_tungstenite::connect_async(web_socket_url)
        .await
        .with_context(|| format!("连接浏览器调试通道失败：{web_socket_url}"))?;

    // 只打开网络事件，不刷新
    let request = json!({ "id": 1, "method": "Network.enable", "params": {} });
    let _ = socket.send(Message::Text(request.to_string().into())).await;

    let mut session = SniffedSession::default();
    let deadline = tokio::time::Instant::now() + timeout;
    // requestId → 是否是招行请求（ExtraInfo 事件没带 url，要靠它关联）
    let mut bank_requests: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        if session.is_complete() {
            break;
        }

        let next = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(next) => next,
            // 超时：把已经采到的交出去
            Err(_) => break,
        };

        let Some(message) = next else { break };
        // 连接被重置（页面在导航）时把已采到的交出去，不当作失败
        let Ok(message) = message else { break };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            _ => continue,
        };

        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // 我们对命令的响应不关心，只看事件（事件没有 id）
        if value.get("id").is_some() {
            continue;
        }

        let method = value
            .get("method")
            .and_then(|item| item.as_str())
            .unwrap_or("");
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "Network.requestWillBeSent" => {
                let url = params
                    .pointer("/request/url")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                if !url_matches(url) {
                    continue;
                }

                session.captured_requests += 1;

                if let Some(request_id) = params.get("requestId").and_then(|item| item.as_str()) {
                    bank_requests.insert(request_id.to_string());
                }

                if session.client_no.is_none() {
                    if let Some(headers) = params.pointer("/request/headers") {
                        session.client_no = header_value(headers, "clientNo");
                    }
                }

                if session.account_uid.is_none() || session.sub_account_no.is_none() {
                    if let Some(body) = params
                        .pointer("/request/postData")
                        .and_then(|item| item.as_str())
                    {
                        let (uid, sub) = extract_account_fields(body);
                        session.account_uid = session.account_uid.or(uid);
                        session.sub_account_no = session.sub_account_no.or(sub);
                    }
                }
            }
            "Network.requestWillBeSentExtraInfo" => {
                // Cookie 是浏览器加的，只能从这个事件里拿到
                let Some(request_id) = params.get("requestId").and_then(|item| item.as_str())
                else {
                    continue;
                };
                if !bank_requests.contains(request_id) || session.cookie.is_some() {
                    continue;
                }
                if let Some(headers) = params.get("headers") {
                    session.cookie = header_value(headers, "cookie");
                }
            }
            _ => {}
        }
    }

    Ok(session)
}

/// 从一次「查流水」请求里捕获到的信息。
#[derive(Debug, Clone, Default)]
pub struct SniffedTransaction {
    pub client_no: Option<String>,
    pub cookie: Option<String>,
    pub account_uid: Option<String>,
    pub sub_account_no: Option<String>,
    /// 原始请求体（里面就有 startDate/endDate，便于对照）。
    pub request_body: Option<String>,
}

impl SniffedTransaction {
    pub fn is_usable(&self) -> bool {
        self.client_no.is_some() && self.account_uid.is_some() && self.sub_account_no.is_some()
    }
}

/// 这条请求是不是「查流水」。
pub fn is_querytrans(url: &str, method: &str) -> bool {
    url_matches(url)
        && method.eq_ignore_ascii_case("POST")
        && url.contains("/accountquery/querytrans")
}

/// **阻塞等待**你点一次「查流水」，把那次请求原样捕获下来。
///
/// 这是 A1 的轻量实现：不起 MITM 代理、不装证书，只监听 biller 自己启动的
/// 那个浏览器。你在窗口里登录后点「查询 / 账户明细」，这里立刻拿到
/// `ClientNo`、`Cookie`、`accountUid`/`subAccountNo` 和请求体。
pub async fn wait_for_transaction(
    web_socket_url: &str,
    timeout: Duration,
) -> Result<SniffedTransaction> {
    let (mut socket, _) = tokio_tungstenite::connect_async(web_socket_url)
        .await
        .with_context(|| format!("连接浏览器调试通道失败：{web_socket_url}"))?;

    let request = json!({ "id": 1, "method": "Network.enable", "params": {} });
    let _ = socket.send(Message::Text(request.to_string().into())).await;

    let mut found = SniffedTransaction::default();
    let mut deadline = tokio::time::Instant::now() + timeout;
    // 招行请求的 requestId：ExtraInfo 事件没带 url，靠它关联 Cookie
    let mut bank_request_id: Option<String> = None;

    loop {
        let next = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(next) => next,
            // 超时（或者命中后等 Cookie 的宽限期到了）
            Err(_) => break,
        };
        let Some(message) = next else { break };
        let Ok(message) = message else { break };

        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("id").is_some() {
            continue;
        }

        let event = value
            .get("method")
            .and_then(|item| item.as_str())
            .unwrap_or("");
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match event {
            "Network.requestWillBeSent" => {
                let url = params
                    .pointer("/request/url")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                let method = params
                    .pointer("/request/method")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                let request_id = params
                    .get("requestId")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");

                if !is_querytrans(url, method) {
                    // 顺手记住别的招行请求，Cookie 可能先到了
                    if url_matches(url) && bank_request_id.is_none() {
                        bank_request_id = Some(request_id.to_string());
                    }
                    continue;
                }

                bank_request_id = Some(request_id.to_string());

                if let Some(headers) = params.pointer("/request/headers") {
                    found.client_no = header_value(headers, "clientNo");
                }
                if let Some(body) = params
                    .pointer("/request/postData")
                    .and_then(|item| item.as_str())
                {
                    found.request_body = Some(body.to_string());
                    let (uid, sub) = extract_account_fields(body);
                    found.account_uid = uid;
                    found.sub_account_no = sub;
                }

                // 命中后只再等 2 秒收 Cookie，不让用户干等
                deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            }
            "Network.requestWillBeSentExtraInfo" => {
                let request_id = params
                    .get("requestId")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                if Some(request_id.to_string()) == bank_request_id && found.cookie.is_none() {
                    if let Some(headers) = params.get("headers") {
                        found.cookie = header_value(headers, "cookie");
                    }
                }
            }
            _ => {}
        }

        if found.is_usable() && found.cookie.is_some() {
            break;
        }
    }

    if !found.is_usable() {
        bail!(
            "等了 {} 秒没等到「查流水」请求。请确认：\n\
             1) 那个窗口里已经登录招商银行网银（能正常看到账户明细）；\n\
             2) 手动点一次「查询」或「账户明细」，让它发出请求。",
            timeout.as_secs()
        );
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_querytrans_request() {
        let url = "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/accountquery/querytrans";

        assert!(is_querytrans(url, "POST"));
        assert!(is_querytrans(url, "post"), "方法大小写不敏感");
        assert!(!is_querytrans(url, "GET"), "只认 POST");
        // 别的招行接口不算
        assert!(!is_querytrans(
            "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/base/queryaccount",
            "POST"
        ));
        // 外部域名不算
        assert!(!is_querytrans(
            "https://example.com/CmbBank_Web_Account/accountquery/querytrans",
            "POST"
        ));
    }

    #[test]
    fn sniffed_transaction_completeness() {
        let mut found = SniffedTransaction::default();
        assert!(!found.is_usable());

        found.client_no = Some("c".to_string());
        found.account_uid = Some("P".to_string());
        let _ = "中间状态也要能判断";
        assert!(!found.is_usable(), "缺 subAccountNo 不算可用");

        found.sub_account_no = Some("1".to_string());
        assert!(found.is_usable());
        assert_eq!(found.cookie, None, "Cookie 是可选增强");
    }

    #[test]
    fn filters_by_the_bank_origin() {
        assert!(url_matches(
            "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/accountquery/querytrans"
        ));
        assert!(!url_matches("https://example.com/x"));
        assert!(!url_matches("https://cbsh.ebank.cmbchina.com.evil.com/x"));
        assert!(!url_matches(""));
    }

    #[test]
    fn reads_headers_case_insensitively() {
        let headers = json!({
            "ClientNo": "67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813",
            "Cookie": "a=base; b=base",
            "content-type": "application/json"
        });

        assert!(header_value(&headers, "clientno").is_some());
        assert!(header_value(&headers, "CLIENTNO").is_some());
        assert_eq!(
            header_value(&headers, "cookie").as_deref(),
            Some("a=base; b=base")
        );
        assert_eq!(header_value(&headers, "missing"), None);
        // 空值不算取到
        let empty = json!({"ClientNo": "   "});
        assert_eq!(header_value(&empty, "clientNo"), None);
    }

    #[test]
    fn extracts_account_fields_from_json_body() {
        // 需求里给的真实请求体
        let body = r#"{"accountUid":"P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_","subAccountNo":"125621564370001","startDate":"20261012","endDate":"20261018"}"#;
        let (uid, sub) = extract_account_fields(body);
        assert_eq!(
            uid.as_deref(),
            Some("P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_")
        );
        assert_eq!(sub.as_deref(), Some("125621564370001"));
    }

    #[test]
    fn extracts_account_fields_from_nested_json() {
        // 响应体里常常嵌好几层，数字型 subAccountNo 也要吃
        let body = r#"{"bizResult":{"data":{"list":[{"account":{"accountUid":"P-abc","subAccountNo":125621564370001}}]}}}"#;
        let (uid, sub) = extract_account_fields(body);
        assert_eq!(uid.as_deref(), Some("P-abc"));
        assert_eq!(sub.as_deref(), Some("125621564370001"));
    }

    #[test]
    fn extracts_account_fields_from_form_encoding() {
        let (uid, sub) = extract_account_fields("accountUid=P-xyz&subAccountNo=999");
        assert_eq!(uid.as_deref(), Some("P-xyz"));
        assert_eq!(sub.as_deref(), Some("999"));
    }

    #[test]
    fn missing_fields_stay_none() {
        assert_eq!(extract_account_fields(""), (None, None));
        assert_eq!(extract_account_fields(r#"{"foo":"bar"}"#), (None, None));
        assert_eq!(extract_account_fields("not json nor form"), (None, None));
    }

    #[test]
    fn session_completeness_and_description() {
        let mut session = SniffedSession::default();
        assert!(!session.is_complete());
        assert_eq!(session.account(), None);

        session.client_no = Some("c".to_string());
        session.account_uid = Some("P-1".to_string());
        session.sub_account_no = Some("2".to_string());
        session.captured_requests = 3;

        assert!(session.is_complete());
        assert_eq!(
            session.account().unwrap(),
            crate::bank::client::BankAccount {
                account_uid: "P-1".to_string(),
                sub_account_no: "2".to_string(),
            }
        );
        assert!(session.describe().contains("捕获 3 条"));
        assert!(session.describe().contains("P-1"));
    }

    #[test]
    fn finds_a_real_browser_on_this_machine() {
        // 开发机上应当装了 Chrome 或 Edge；没有也只是返回 None，不算失败
        if let Some(path) = find_browser() {
            assert!(path.is_file(), "找到的浏览器路径应存在：{}", path.display());
        }
    }

    #[test]
    fn builds_launch_args_with_its_own_profile() {
        let args = browser_args(9222, Path::new("C:/Users/me/.biller/browser"), ORIGIN);

        assert_eq!(args[0], "--remote-debugging-port=9222");
        assert!(
            args[1].starts_with("--user-data-dir="),
            "必须用独立配置目录，否则 Chrome 不给开调试端口"
        );
        assert!(args[1].contains("browser"));
        assert_eq!(args.last().unwrap(), ORIGIN);
    }

    #[test]
    fn parses_the_target_list() {
        let text = r#"[
            {"id":"a","url":"about:blank","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/a"},
            {"id":"b","url":"https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/account/querytrans","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/b"},
            {"id":"c","url":"https://example.com"}
        ]"#;

        let targets = parse_targets(text).unwrap();
        assert_eq!(targets.len(), 2, "没有 WebSocket 地址的条目应跳过");
        assert_eq!(
            targets[1].url,
            "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/account/querytrans"
        );
    }

    #[test]
    fn rejects_malformed_target_lists() {
        assert!(parse_targets("not json").is_err());
        assert!(parse_targets(r#"{"a":1}"#).is_err(), "不是数组应报错");
    }

    #[test]
    fn picks_the_bank_tab_and_ignores_others() {
        let targets = vec![
            TargetInfo {
                url: "about:blank".to_string(),
                web_socket_url: "ws://1".to_string(),
            },
            TargetInfo {
                url: format!("{ORIGIN}/CmbBank_Web_Account/account/querytrans"),
                web_socket_url: "ws://2".to_string(),
            },
        ];

        let picked = pick_bank_target(&targets).expect("应挑到招行页面");
        assert_eq!(picked.web_socket_url, "ws://2");

        // 没有招行页面时返回 None
        let none = vec![TargetInfo {
            url: "https://example.com".to_string(),
            web_socket_url: "ws://3".to_string(),
        }];
        assert!(pick_bank_target(&none).is_none());
    }

    #[test]
    fn extracts_client_no_by_value_shape() {
        let raw = r#"[
            {"where":"localStorage","key":"theme","value":"dark"},
            {"where":"localStorage","key":"deviceId","value":"67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813"},
            {"where":"sessionStorage","key":"token","value":"abc"}
        ]"#;

        assert_eq!(
            parse_client_no(raw).as_deref(),
            Some("67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813")
        );
    }

    #[test]
    fn extracts_client_no_by_key_name_as_fallback() {
        // 形状不认识，但键名对得上
        let raw = r#"[
            {"where":"localStorage","key":"ClientNo","value":"some-opaque-value"}
        ]"#;
        assert_eq!(parse_client_no(raw).as_deref(), Some("some-opaque-value"));
    }

    #[test]
    fn returns_none_when_nothing_looks_like_client_no() {
        assert_eq!(parse_client_no("[]"), None);
        assert_eq!(parse_client_no("not json"), None);
        assert_eq!(
            parse_client_no(r#"[{"where":"localStorage","key":"a","value":"b"}]"#),
            None
        );
    }

    #[test]
    fn scan_js_reads_both_storages() {
        let js = client_no_scan_js();
        assert!(js.contains("localStorage"));
        assert!(js.contains("sessionStorage"));
        assert!(js.contains("JSON.stringify"), "返回值要能被打包拿回来");
    }

    #[test]
    fn user_data_dir_sits_next_to_the_database() {
        let directory = user_data_dir();
        assert_eq!(directory.file_name().unwrap(), "browser");
        assert!(
            directory.starts_with(crate::common::db::default_sqlite_path().parent().unwrap()),
            "应与本地库在同一个 .biller 目录下：{}",
            directory.display()
        );
    }
}
