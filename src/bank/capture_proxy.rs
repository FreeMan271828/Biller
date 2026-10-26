//! 本地 MITM 捕获代理：**完全无感**捕获你日常浏览器的流量。
//!
//! 用 `hudsucker` 起一个本地 MITM 代理，证书链用 [`proxy_ca`](crate::bank::proxy_ca)
//! 那份本地 CA 动态签发。你只要：
//!
//! 1. `biller sync ca-init` 生成 CA，并按提示把它装进「当前用户的受信任根证书」；
//! 2. 把浏览器/系统代理指到 `127.0.0.1:9080`；
//! 3. `biller sync proxy-capture` 阻塞等待；
//! 4. 在**你自己的日常浏览器**里点一次「查询」——这里立刻拿到那次请求的
//!    `ClientNo`、`Cookie`、`accountUid`、`subAccountNo`，捕获完代理自动关闭。
//!
//! 与 A1-lite（`sync capture`）的区别：A1-lite 只能监听 biller 自己启动的那个浏览器；
//! 这里能捕获**任何**走该代理的客户端。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hudsucker::{
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
    certificate_authority::RcgenAuthority, hyper::Request, rustls::crypto::aws_lc_rs,
};
use rcgen::{Issuer, KeyPair};
use tokio::sync::oneshot;

use crate::bank::browser::{extract_account_fields, header_value, url_matches};
use crate::bank::capture::CapturedRequest;
use crate::bank::proxy_ca;

/// 命中条件：招行域名 + POST + 查流水接口。
///
/// 代理会对所有 host 做 MITM，所以这里必须严格过滤，只认真正的查流水请求。
pub fn should_capture(url: &str, method: &str) -> bool {
    if !method.eq_ignore_ascii_case("POST") {
        return false;
    }

    // 有 scheme 时按域名边界严格匹配；有的实现只给 path，就退一步看路径
    let host_ok =
        url_matches(url) || (!url.contains("://") && url.contains("/CmbBank_Web_Account/"));
    host_ok && url.contains("/accountquery/querytrans")
}

/// 请求头 + 请求体 → 捕获结果。
pub fn to_captured(headers: &serde_json::Value, body: Option<&str>) -> CapturedRequest {
    let client_no = header_value(headers, "clientNo");
    let cookie = header_value(headers, "cookie");
    let (account_uid, sub_account_no) = body.map(extract_account_fields).unwrap_or((None, None));

    CapturedRequest {
        client_no,
        cookie,
        account_uid,
        sub_account_no,
    }
}

/// 代理里挂的处理器：命中就把请求翻出来，放进 channel；不命中就原样放行。
#[derive(Clone)]
struct CaptureHandler {
    sink: Arc<Mutex<Option<oneshot::Sender<CapturedRequest>>>>,
}

impl HttpHandler for CaptureHandler {
    async fn handle_request(
        &mut self,
        _context: &HttpContext,
        request: Request<Body>,
    ) -> RequestOrResponse {
        let method = request.method().as_str().to_string();
        let url = request.uri().to_string();
        let host = request
            .uri()
            .host()
            .or_else(|| {
                request
                    .headers()
                    .get("host")
                    .and_then(|value| value.to_str().ok())
            })
            .unwrap_or("")
            .to_string();

        // 拼一个尽量完整的 URL 供匹配：hudsucker 一般给绝对 URI，
        // 只给 path 时用 host 头补上。
        let full = if url.contains("://") {
            url.clone()
        } else {
            format!("https://{host}{url}")
        };

        if !should_capture(&full, &method) {
            return RequestOrResponse::Request(request);
        }

        let mut headers = serde_json::Map::new();
        for (name, value) in request.headers() {
            if let Ok(text) = value.to_str() {
                headers.insert(
                    name.as_str().to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        }

        let (parts, body) = request.into_parts();
        let bytes = match http_body_util::BodyExt::collect(body).await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => Default::default(),
        };
        let body_text = String::from_utf8_lossy(&bytes).to_string();

        let captured = to_captured(&serde_json::Value::Object(headers), Some(&body_text));
        if let Some(sink) = self.sink.lock().ok().and_then(|mut slot| slot.take()) {
            let _ = sink.send(captured);
        }

        // 原样放行，不打断用户的操作
        RequestOrResponse::Request(Request::from_parts(parts, Body::from(bytes)))
    }
}

/// 起代理并阻塞等待一次「查流水」，捕获后关闭代理返回。
///
/// 超时返回可操作的中文错误。成功返回后代理任务会被 abort。
pub async fn capture(port: u16, timeout: Duration) -> Result<CapturedRequest> {
    // 证书：没有就生成（提示语由 CLI 负责）
    let ca = proxy_ca::ensure_ca()?;

    let key_pair = KeyPair::from_pem(&ca.key_pem).context(
        "解析本地 CA 私钥失败；可以删掉 %USERPROFILE%\\.biller\\proxy 后重新 `biller sync ca-init`",
    )?;
    let issuer = Issuer::from_ca_cert_pem(&ca.cert_pem, key_pair)
        .context("解析本地 CA 证书失败；重新 `biller sync ca-init` 试试")?;

    let provider = aws_lc_rs::default_provider();
    // 有些 rustls 版本要求全局 provider，重复安装会返回 Err，忽略即可
    let _ = provider.clone().install_default();

    let authority = RcgenAuthority::new(issuer, 1_000, provider.clone());

    let (sender, receiver) = oneshot::channel();
    let handler = CaptureHandler {
        sink: Arc::new(Mutex::new(Some(sender))),
    };

    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let proxy = Proxy::builder()
        .with_addr(address)
        .with_ca(authority)
        .with_rustls_connector(provider)
        .with_http_handler(handler)
        .build()
        .map_err(|error| anyhow::anyhow!("启动捕获代理失败（端口 {port} 可能被占用）：{error}"))?;

    let task = tokio::spawn(async move {
        let _ = proxy.start().await;
    });

    let result = tokio::time::timeout(timeout, receiver).await;
    task.abort();

    match result {
        Ok(Ok(captured)) => Ok(captured),
        Ok(Err(_)) => bail!("捕获代理提前结束，请重试"),
        Err(_) => bail!(
            "等了 {} 秒没等到「查流水」请求。请确认：\n\
             1) 浏览器或系统代理已指向 127.0.0.1:{port}；\n\
             2) 已登录招商银行网银，并在页面里点了一次「查询」/「账户明细」。",
            timeout.as_secs()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_URL: &str =
        "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/accountquery/querytrans";

    #[test]
    fn matches_only_the_real_querytrans_request() {
        assert!(should_capture(REAL_URL, "POST"));
        assert!(should_capture(REAL_URL, "post"), "方法大小写不敏感");

        assert!(!should_capture(REAL_URL, "GET"), "只认 POST");
        assert!(
            !should_capture(
                "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/base/queryaccount",
                "POST"
            ),
            "别的招行接口不算"
        );
        assert!(
            !should_capture(
                "https://cbsh.ebank.cmbchina.com.evil.com/CmbBank_Web_Account/accountquery/querytrans",
                "POST"
            ),
            "伪造域名必须挡住"
        );
        assert!(!should_capture("https://example.com/x", "POST"));
    }

    #[test]
    fn accepts_path_only_uris() {
        // 有的实现只给 path：用 host 头拼出来的完整 URL 也能认
        assert!(should_capture(
            "https://cbsh.ebank.cmbchina.com/CmbBank_Web_Account/accountquery/querytrans",
            "POST"
        ));
    }

    #[test]
    fn extracts_session_from_headers_and_body() {
        let headers = serde_json::json!({
            "ClientNo": "67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813",
            "Cookie": "33dd74597660bfb607ef1d8e314e6df2=base; 22e9dfe6097c69932d101c0ce266d29f=base",
            "Content-Type": "application/json"
        });
        let body = r#"{"accountUid":"P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_","subAccountNo":"125621564370001","startDate":"20261012","endDate":"20261018"}"#;

        let captured = to_captured(&headers, Some(body));

        assert_eq!(
            captured.client_no.as_deref(),
            Some("67db80ad-3f05-4933-9be9-8b3445c023dc-1792331235813")
        );
        assert_eq!(
            captured.account_uid.as_deref(),
            Some("P-DKJ43rfy*Anf9tAIgUFCScwRGkcs9c9IdP5bBZm1s_")
        );
        assert_eq!(captured.sub_account_no.as_deref(), Some("125621564370001"));
        assert!(captured.cookie.is_some());
        assert!(captured.is_usable(), "三值齐全应判定为可用");
    }

    #[test]
    fn missing_body_leaves_account_empty() {
        let headers = serde_json::json!({"ClientNo": "abc"});
        let captured = to_captured(&headers, None);

        assert_eq!(captured.client_no.as_deref(), Some("abc"));
        assert!(captured.account().is_none());
        assert!(!captured.is_usable());
    }

    #[tokio::test]
    async fn times_out_with_a_helpful_error() {
        // 会用真实 CA 目录：与 proxy_ca 的用例串行，避免互相删除证书文件
        let _guard = crate::bank::proxy_ca::test_serial_lock();

        // 用一个几乎肯定空闲的端口，0.3 秒超时：应返回「没等到」的中文错误
        let error = capture(0, Duration::from_millis(300))
            .await
            .expect_err("没有客户端，必然超时");

        let text = format!("{error}");
        assert!(text.contains("没等到"), "{text}");
        assert!(text.contains("代理"), "{text}");
    }
}
