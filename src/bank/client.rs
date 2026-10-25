//! 构造 `querytrans` 请求，以及在页面里执行它的那段 JS。
//!
//! 为什么不自己拼 Cookie 发 HTTP：那些凭证都在浏览器的登录会话里，而且
//! Cookie 通常几十分钟就过期。做法是**让浏览器自己去发**——
//! Cookie、Origin、`Sec-Fetch-*` 全由浏览器补上，我们只负责把 body 和
//! 业务请求头写对。这样就不需要解密 cookie、也不需要处理跨域。

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::bank::model::SyncWindow;

/// 目标接口路径。
pub const TRANS_PATH: &str = "/CmbBank_Web_Account/accountquery/querytrans";

/// 目标站点。
pub const ORIGIN: &str = "https://cbsh.ebank.cmbchina.com";

/// 招行网页版固定的几个业务头，从真实请求里抄下来的。
const AUTH_TYPE: &str = "A";
const DEVICE_TYPE: &str = "R";
const LOGIN_VERSION: &str = "R";
const PRO_VERSION: &str = "MG7.8.1.1";

/// 一个账户（对应一张卡的子账户）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankAccount {
    /// 页面里的账户标识，形如 `P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_`。
    pub account_uid: String,
    /// 子账户号，形如 `125621564370001`。
    pub sub_account_no: String,
}

impl BankAccount {
    /// 界面上不要暴露完整账户号。
    pub fn masked(&self) -> String {
        let digits: String = self
            .sub_account_no
            .chars()
            .filter(|character| character.is_ascii_digit())
            .collect();

        if digits.len() <= 4 {
            return format!("尾号 {digits}");
        }

        format!("尾号 {}", &digits[digits.len() - 4..])
    }
}

/// `querytrans` 的请求体。
pub fn querytrans_body(account: &BankAccount, window: &SyncWindow) -> serde_json::Value {
    json!({
        "accountUid": account.account_uid,
        "subAccountNo": account.sub_account_no,
        "startDate": window.start_text(),
        "endDate": window.end_text(),
    })
}

/// 在页面里执行请求的 JS。
///
/// 返回的是一个 JSON 字符串：`{"status":200,"text":"...接口原文..."}`，
/// 解析交给 [`unwrap_js_result`]。`request-timestamp` 在页面里用 `Date.now()`
/// 现取，所以每次调用都是新的。
pub fn querytrans_js(account: &BankAccount, window: &SyncWindow, client_no: &str) -> String {
    let body = querytrans_body(account, window);
    // JSON 字符串同时也是合法的 JS 字符串字面量，这里直接嵌进去
    let body_literal = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    let client_no_literal = serde_json::to_string(client_no).unwrap_or_else(|_| "\"\"".to_string());

    format!(
        r#"(async () => {{
  const response = await fetch({path}, {{
    method: "POST",
    headers: {{
      "Content-Type": "application/json",
      "Accept": "application/json, text/plain, */*",
      "authType": {auth_type},
      "deviceType": {device_type},
      "loginVersion": {login_version},
      "proVersion": {pro_version},
      "X-Requested-With": "XMLHttpRequest",
      "cachetimestamp": "",
      "ClientNo": {client_no},
      "request-timestamp": String(Date.now())
    }},
    body: {body},
    credentials: "include"
  }});
  return JSON.stringify({{ status: response.status, text: await response.text() }});
}})()"#,
        path = serde_json::to_string(TRANS_PATH).unwrap(),
        auth_type = serde_json::to_string(AUTH_TYPE).unwrap(),
        device_type = serde_json::to_string(DEVICE_TYPE).unwrap(),
        login_version = serde_json::to_string(LOGIN_VERSION).unwrap(),
        pro_version = serde_json::to_string(PRO_VERSION).unwrap(),
        client_no = client_no_literal,
        body = body_literal,
    )
}

/// 把 [`querytrans_js`] 的返回（`Runtime.evaluate` 的字符串结果）拆开。
pub fn unwrap_js_result(text: &str) -> Result<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("页面没有返回任何内容（可能标签页已关闭或未登录）");
    }

    let value: serde_json::Value =
        serde_json::from_str(trimmed).context("页面脚本的返回值不是 JSON")?;

    let status = value.get("status").and_then(|item| item.as_i64());
    let body = value
        .get("text")
        .and_then(|item| item.as_str())
        .context("页面脚本的返回值缺少 text 字段")?;

    match status {
        Some(200) => Ok(body.to_string()),
        Some(other) => bail!(
            "银行接口返回 HTTP {other}（{}）",
            body.chars().take(80).collect::<String>()
        ),
        None => Ok(body.to_string()),
    }
}

/// 判断一段字符串是不是 `ClientNo`：UUID + 13 位毫秒时间戳。
///
/// 真实值形如 `67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813`。
pub fn looks_like_client_no(value: &str) -> bool {
    let value = value.trim();

    // 36 位 UUID + '-' + 13 位时间戳 = 50
    if value.len() != 50 {
        return false;
    }

    let Some((uuid, stamp)) = value.rsplit_once('-') else {
        return false;
    };

    if uuid.len() != 36 || !stamp.chars().all(|character| character.is_ascii_digit()) {
        return false;
    }
    if stamp.len() != 13 {
        return false;
    }

    // UUID 部分：8-4-4-4-12，全十六进制
    let groups: Vec<&str> = uuid.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    let sizes = [8, 4, 4, 4, 12];
    groups
        .iter()
        .zip(sizes)
        .all(|(group, size)| group.len() == size && group.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn account() -> BankAccount {
        BankAccount {
            account_uid: "P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_".to_string(),
            sub_account_no: "125621564370001".to_string(),
        }
    }

    fn window() -> SyncWindow {
        let start = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        SyncWindow {
            start,
            end,
            label: "本周",
        }
    }

    #[test]
    fn builds_the_request_body() {
        let body = querytrans_body(&account(), &window());

        assert_eq!(
            body["accountUid"],
            "P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_"
        );
        assert_eq!(body["subAccountNo"], "125621564370001");
        assert_eq!(body["startDate"], "20261012");
        assert_eq!(body["endDate"], "20261018");
    }

    #[test]
    fn js_carries_path_body_and_client_no() {
        let js = querytrans_js(&account(), &window(), "67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813");

        assert!(js.contains(TRANS_PATH), "应包含接口路径");
        assert!(js.contains("\"startDate\":\"20261012\""), "请求体应嵌进去");
        assert!(
            js.contains("67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813"),
            "ClientNo 应传进来"
        );
        assert!(js.contains("credentials"), "要带上 Cookie");
        assert!(js.contains("Date.now()"), "时间戳在页面里现取");
        assert!(js.starts_with("(async () =>"), "应是一段可执行表达式");
    }

    #[test]
    fn js_keeps_special_characters_in_client_no() {
        // ClientNo 里带引号时不能把 JS 拼坏
        let js = querytrans_js(&account(), &window(), "a\"b\\c");
        assert!(js.contains(r#""ClientNo": "a\"b\\c""#), "应正确转义：{js}");
    }

    #[test]
    fn unwraps_successful_js_result() {
        let text = r#"{"status":200,"text":"{\"sysCode\":200}"}"#;
        assert_eq!(unwrap_js_result(text).unwrap(), r#"{"sysCode":200}"#);
    }

    #[test]
    fn surfaces_non_200_http_status() {
        let text = r#"{"status":401,"text":"unauthorized"}"#;
        let error = unwrap_js_result(text).expect_err("应报错");
        assert!(error.to_string().contains("HTTP 401"), "{error}");
    }

    #[test]
    fn rejects_empty_or_malformed_js_result() {
        assert!(unwrap_js_result("").is_err());
        assert!(unwrap_js_result("not json").is_err());
        assert!(unwrap_js_result(r#"{"status":200}"#).is_err(), "缺 text 字段");
    }

    #[test]
    fn recognizes_client_no_shape() {
        assert!(looks_like_client_no(
            "67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813"
        ));
        // 前后空白容忍
        assert!(looks_like_client_no(
            " 67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813 "
        ));

        assert!(!looks_like_client_no(""), "空串");
        assert!(!looks_like_client_no("67db80ad"), "太短");
        // 时间戳位数不对
        assert!(!looks_like_client_no(
            "67db80ad-3f05-4933-9be9-8b3445c023dc-17923312358"
        ));
        // UUID 段长度不对
        assert!(!looks_like_client_no(
            "67db80a-3f05-4933-9be9-8b3445c023dc-1792331235813"
        ));
        // 时间戳里混了字母
        assert!(!looks_like_client_no(
            "67db80ad-3f05-4933-9be9-8b3445c023dc-17923312358a"
        ));
    }

    #[test]
    fn masks_account_numbers() {
        assert_eq!(account().masked(), "尾号 0001");
        assert_eq!(
            BankAccount {
                account_uid: "x".to_string(),
                sub_account_no: "12".to_string(),
            }
            .masked(),
            "尾号 12"
        );
    }
}
