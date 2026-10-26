//! 方案 A3：把你从浏览器 / Proxyman 里复制出来的请求粘进来，解析出会话信息。
//!
//! 支持两种粘贴格式：
//! 1. Chrome DevTools 的 `Copy as cURL`（bash / cmd 都认）
//! 2. 原始 HTTP 请求块（`POST /… HTTP/1.1` 那一坨，需求里最早给的就是这种）
//!
//! 解析出来的东西存到 `%USERPROFILE%\.biller\bank.toml`，之后就能直接重放，
//! 直到会话过期再粘一次。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::bank::browser::extract_account_fields;
use crate::bank::client::{self, BankAccount};
use crate::bank::model::{self, BankTransaction, SyncWindow};

/// 保存捕获结果的文件。
const FILE_NAME: &str = "bank.toml";

/// 从请求里解析出来的会话信息。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapturedRequest {
    pub client_no: Option<String>,
    pub cookie: Option<String>,
    pub account_uid: Option<String>,
    pub sub_account_no: Option<String>,
}

impl CapturedRequest {
    /// 拼成账户对象；缺一半返回 None。
    pub fn account(&self) -> Option<BankAccount> {
        Some(BankAccount {
            account_uid: self.account_uid.clone()?,
            sub_account_no: self.sub_account_no.clone()?,
        })
    }

    /// 重放至少要 ClientNo + 账户 + Cookie。
    pub fn is_usable(&self) -> bool {
        self.client_no.is_some() && self.account().is_some() && self.cookie.is_some()
    }

    pub fn describe(&self) -> String {
        format!(
            "ClientNo={}｜accountUid={}｜subAccountNo={}｜Cookie={}",
            self.client_no.as_deref().unwrap_or("未取到"),
            self.account_uid.as_deref().unwrap_or("未取到"),
            self.sub_account_no.as_deref().unwrap_or("未取到"),
            match &self.cookie {
                Some(cookie) => format!("{} 个键", cookie.matches('=').count().max(1)),
                None => "未取到".to_string(),
            }
        )
    }
}

/// 粘贴内容 → 会话信息。
pub fn parse(text: &str) -> Result<CapturedRequest> {
    if text.trim().is_empty() {
        bail!("粘贴内容是空的");
    }

    let headers = collect_headers(text);
    let body = find_body(text);

    let mut captured = CapturedRequest::default();

    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("clientno") {
            captured.client_no = Some(value.clone());
        } else if name.eq_ignore_ascii_case("cookie") {
            captured.cookie = Some(value.clone());
        }
    }

    // 账号优先从请求体里挖；没有就把整坨文本扫一遍（响应体/其它字段也能命中）
    let from_body = body
        .as_deref()
        .map(extract_account_fields)
        .unwrap_or((None, None));
    captured.account_uid = from_body.0;
    captured.sub_account_no = from_body.1;

    if captured.account_uid.is_none() || captured.sub_account_no.is_none() {
        let (uid, sub) = extract_account_fields(text);
        captured.account_uid = captured.account_uid.or(uid);
        captured.sub_account_no = captured.sub_account_no.or(sub);
    }

    if captured.client_no.is_none() && captured.account_uid.is_none() {
        bail!(
            "没能从粘贴内容里找到 ClientNo 或 accountUid。\n\
             请在浏览器 DevTools 的 Network 面板里右键那条 querytrans 请求 → \
             Copy → Copy as cURL，再粘进来"
        );
    }

    Ok(captured)
}

/// 收集 `Name: value` 形式的头，cURL 的 `-H` 和原始 HTTP 都认。
fn collect_headers(text: &str) -> Vec<(String, String)> {
    let mut headers = Vec::new();

    // 1) cURL 的 -H / --header
    for raw in curl_arguments(text) {
        let argument = unquote(&raw);
        if let Some((name, value)) = argument.split_once(':') {
            let name = name.trim();
            if !name.is_empty() && !name.contains(' ') {
                headers.push((name.to_string(), value.trim().to_string()));
            }
        }
    }

    // 2) 原始 HTTP 的 `Name: value` 行（第一行是 `POST /path HTTP/1.1`，跳过）
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("curl") {
            continue;
        }
        if line.contains(" HTTP/") {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if name.is_empty() || name.contains(' ') || value.trim().is_empty() {
                continue;
            }
            let already = headers
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(name));
            if !already {
                headers.push((name.to_string(), value.trim().to_string()));
            }
        }
    }

    headers
}

/// 把 cURL 的命令行参数粗略切出来（支持单双引号、`-H` 与 `--header`）。
fn curl_arguments(text: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for character in text.chars() {
        match quote {
            Some(active) if character == active => quote = None,
            Some(_) => current.push(character),
            None => match character {
                '\'' | '"' => quote = Some(character),
                ' ' | '\n' | '\r' | '\t' => {
                    if !current.is_empty() {
                        arguments.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(character),
            },
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }

    // 只留 -H/--header 后面的那一串
    let mut headers = Vec::new();
    let mut expect = false;
    for argument in arguments {
        if argument == "-H" || argument == "--header" {
            expect = true;
            continue;
        }
        if expect {
            headers.push(argument);
            expect = false;
        }
    }

    headers
}

fn unquote(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .trim_start_matches(['\'', '"'])
        .trim_end_matches(['\'', '"'])
        .replace("\\\"", "\"")
}

/// 找请求体：cURL 的 `-d/--data*/--data-raw`，或原始 HTTP 空行之后的部分。
fn find_body(text: &str) -> Option<String> {
    // cURL：参数里找 --data / -d
    let arguments: Vec<String> = split_arguments(text);
    let mut expect = false;
    for argument in arguments {
        if matches!(
            argument.as_str(),
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-urlencode"
        ) {
            expect = true;
            continue;
        }
        if expect {
            return Some(unquote(&argument));
        }
    }

    // 原始 HTTP：空行之后
    if let Some((_, body)) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
    {
        let body = body.trim();
        if !body.is_empty() {
            return Some(body.to_string());
        }
    }

    // 兜底：整段里第一个 `{` 开始的 JSON
    let trimmed = text.trim();
    let start = trimmed.find('{')?;
    Some(trimmed[start..].to_string())
}

/// 按引号规则切参数（保留引号内空格）。
fn split_arguments(text: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for character in text.chars() {
        match quote {
            // 关引号
            Some(active) if character == active => quote = None,
            // 引号内的内容原样保留（含 `\`，留给 unquote 还原）
            Some(_) => current.push(character),
            None => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    current.push(character);
                }
                ' ' | '\n' | '\r' | '\t' => {
                    if !current.is_empty() {
                        arguments.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(character),
            },
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }

    arguments
}

// ---------------- 存取 ----------------

/// 配置文件路径：`%USERPROFILE%\.biller\bank.toml`。
pub fn store_path() -> PathBuf {
    crate::common::db::default_sqlite_path().with_file_name(FILE_NAME)
}

/// 保存（覆盖）。
pub fn save(captured: &CapturedRequest) -> Result<PathBuf> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
    }

    let mut text = String::from(
        "# 由 `biller sync paste` 生成：从浏览器请求里捕获的会话信息。\n\
         # 含登录凭证，请勿提交到任何仓库；会话过期后重新粘贴即可。\n",
    );
    text.push_str(&format!(
        "client_no = \"{}\"\n",
        escape(captured.client_no.as_deref().unwrap_or(""))
    ));
    text.push_str(&format!(
        "cookie = \"{}\"\n",
        escape(captured.cookie.as_deref().unwrap_or(""))
    ));
    text.push_str(&format!(
        "account_uid = \"{}\"\n",
        escape(captured.account_uid.as_deref().unwrap_or(""))
    ));
    text.push_str(&format!(
        "sub_account_no = \"{}\"\n",
        escape(captured.sub_account_no.as_deref().unwrap_or(""))
    ));
    text.push_str(&format!(
        "updated_at = \"{}\"\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
    ));

    std::fs::write(&path, text).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(path)
}

/// 读回上次保存的会话；没有就返回 None。
pub fn load() -> Result<Option<CapturedRequest>> {
    let path = store_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };

    let mut captured = CapturedRequest::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unescape(value.trim());
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "client_no" => captured.client_no = Some(value),
            "cookie" => captured.cookie = Some(value),
            "account_uid" => captured.account_uid = Some(value),
            "sub_account_no" => captured.sub_account_no = Some(value),
            _ => {}
        }
    }

    if captured.client_no.is_none() && captured.account_uid.is_none() {
        return Ok(None);
    }
    Ok(Some(captured))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

fn unescape(value: &str) -> String {
    let trimmed = value.trim();
    let stripped = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    stripped.replace("\\\\", "\\").replace("\\\"", "\"")
}

// ---------------- 重放 ----------------

/// 用捕获到的会话直连重放查询（不经过浏览器）。
///
/// 会话过期（返回登录页/业务码非 200）时错误信息里会明确提示重新粘贴。
pub async fn replay(
    captured: &CapturedRequest,
    account: &BankAccount,
    window: &SyncWindow,
) -> Result<Vec<BankTransaction>> {
    let client_no = captured
        .client_no
        .as_deref()
        .context("还没捕获到 ClientNo，请先跑 `biller sync paste`")?;
    let cookie = captured
        .cookie
        .as_deref()
        .context("还没捕获到 Cookie，请先跑 `biller sync paste`")?;

    let url = format!("{}{}", client::ORIGIN, client::TRANS_PATH);
    let body = client::querytrans_body(account, window).to_string();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("创建 HTTP 客户端失败")?;

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("authType", "A")
        .header("deviceType", "R")
        .header("loginVersion", "R")
        .header("proVersion", "MG7.8.1.1")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("ClientNo", client_no)
        .header("Cookie", cookie)
        .header("Origin", client::ORIGIN)
        .header(
            "Referer",
            format!("{}/CmbBank_Web_Account/account/querytrans", client::ORIGIN),
        )
        .header(
            "request-timestamp",
            chrono::Utc::now().timestamp_millis().to_string(),
        )
        .body(body)
        .send()
        .await
        .context("请求招商银行接口失败（网络不通或域名不可达）")?;

    let status = response.status();
    let text = response.text().await.context("读取接口返回失败")?;

    if !status.is_success() {
        bail!(
            "接口返回 HTTP {status}。会话可能已过期，请重新复制一次请求并运行 `biller sync paste`。\n{}",
            text.chars().take(200).collect::<String>()
        );
    }

    model::parse_querytrans_response(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chrome DevTools 的 `Copy as cURL`（bash 风格，单引号 + `\` 续行）。
    const CURL: &str = r#"curl 'https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/accountquery/querytrans' \
  -H 'accept: application/json, text/plain, */*' \
  -H 'authType: A' \
  -H 'ClientNo: 67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813' \
  -H 'Content-Type: application/json' \
  -H 'Cookie: 33dd74597660bfb607ef1d8e314e6df2=base; 22e9dfe6097c69932d101c0ce266d29f=base' \
  --data-raw '{"accountUid":"P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_","subAccountNo":"125621564370001","startDate":"20261012","endDate":"20261018"}'"#;

    /// 需求里最早贴的原始 HTTP 请求块。
    const RAW: &str = r#"POST /CmbBank_Web_Account/accountquery/querytrans HTTP/1.1
Host: cbsh.ebank.cmbchina.com
Connection: keep-alive
authType: A
deviceType: R
request-timestamp: 1792331397783
ClientNo: 67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813
Content-Type: application/json
Cookie: 33dd74597660bfb607ef1d8e314e6df2=base; 22e9dfe6097c69932d101c0ce266d29f=base
Origin: https://cbsh.ebank.cmbchina.com

{"accountUid": "P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_", "subAccountNo": "125621564370001", "startDate": "20261012", "endDate": "20261018"}"#;

    fn assert_complete(captured: &CapturedRequest) {
        assert_eq!(
            captured.client_no.as_deref(),
            Some("67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813")
        );
        assert_eq!(
            captured.account_uid.as_deref(),
            Some("P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_")
        );
        assert_eq!(captured.sub_account_no.as_deref(), Some("125621564370001"));
        assert!(
            captured
                .cookie
                .as_deref()
                .unwrap_or("")
                .contains("33dd74597660bfb607ef1d8e314e6df2"),
            "Cookie 应被抓到"
        );
        assert!(captured.is_usable(), "三个值齐全才算可用");
    }

    #[test]
    fn parses_a_chrome_curl() {
        assert_complete(&parse(CURL).expect("应能解析 cURL"));
    }

    #[test]
    fn parses_a_raw_http_request() {
        assert_complete(&parse(RAW).expect("应能解析原始请求"));
    }

    #[test]
    fn parses_curl_without_body() {
        // 只有头没有 body：ClientNo 能拿到，账户拿不到
        let text =
            r#"curl "https://cbsh.ebank.cmbchina.com/x" -H "ClientNo: abc-123" -H "Cookie: a=1""#;
        let captured = parse(text).unwrap();
        assert_eq!(captured.client_no.as_deref(), Some("abc-123"));
        assert_eq!(captured.cookie.as_deref(), Some("a=1"));
        assert!(captured.account().is_none());
        assert!(!captured.is_usable(), "缺账户不算可用");
    }

    #[test]
    fn empty_or_unrelated_input_is_rejected() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
        assert!(parse("hello world").is_err());
    }

    #[test]
    fn escapes_and_unescapes_round_trip() {
        let original = r#"a"b\c; d=e"#;
        assert_eq!(unescape(&format!("\"{}\"", escape(original))), original);
    }

    #[test]
    fn describe_mentions_missing_fields() {
        let captured = CapturedRequest::default();
        let text = captured.describe();
        assert!(text.contains("未取到"), "{text}");
    }

    #[test]
    fn cookie_key_count_is_shown() {
        let captured = CapturedRequest {
            client_no: Some("c".to_string()),
            cookie: Some("a=1; b=2".to_string()),
            account_uid: Some("P".to_string()),
            sub_account_no: Some("1".to_string()),
        };
        assert!(
            captured.describe().contains("2 个键"),
            "{}",
            captured.describe()
        );
    }
}
