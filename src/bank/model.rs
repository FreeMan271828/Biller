//! 银行流水同步的数据结构与纯逻辑。
//!
//! 这里不碰网络、不碰数据库，全部是可以用样例 JSON 直接单测的部分：
//! 同步窗口怎么算、接口返回怎么解析、去重指纹怎么生成、店家名怎么归一化。

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::bill::model::BillKind;
use crate::common::types::AmountType;

/// 招商银行流水查询接口的返回里，`dateTime` 用这个格式。
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// 同步窗口：拉哪一段时间的流水。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncWindow {
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// 界面上显示的名字（今日 / 本周）。
    pub label: &'static str,
}

impl SyncWindow {
    /// 今日：只今天一天。
    pub fn today(today: NaiveDate) -> Self {
        Self {
            start: today,
            end: today,
            label: "今日",
        }
    }

    /// 本周：本周一 ~ min(今天, 本周日)，但**起点要在本地最新一条账单之后**，
    /// 免得把已经记过的日子再拉一遍。
    ///
    /// 返回 `None` 表示本周本地已经记到最新，没有需要同步的区间。
    pub fn this_week(today: NaiveDate, latest_local: Option<NaiveDate>) -> Option<Self> {
        // `num_days_from_monday()`：周一 = 0，周日 = 6
        let monday = today - Duration::days(i64::from(today.weekday().num_days_from_monday()));
        let sunday = monday + Duration::days(6);

        // 未来的日子不看
        let end = today.min(sunday);
        let start = match latest_local {
            Some(latest) => monday.max(latest + Duration::days(1)),
            None => monday,
        };

        if start > end {
            return None;
        }

        Some(Self {
            start,
            end,
            label: "本周",
        })
    }

    /// 接口要的 `YYYYMMDD`。
    pub fn start_text(&self) -> String {
        self.start.format("%Y%m%d").to_string()
    }

    pub fn end_text(&self) -> String {
        self.end.format("%Y%m%d").to_string()
    }

    /// `20261012–20261018`，给提示语用。
    pub fn describe(&self) -> String {
        format!("{}–{}", self.start_text(), self.end_text())
    }

    /// 天数（含首尾）。
    pub fn days(&self) -> i64 {
        (self.end - self.start).num_days() + 1
    }
}

/// 一条银行流水，已经整理成记账能直接用的形状。
#[derive(Debug, Clone, PartialEq)]
pub struct BankTransaction {
    /// 交易时刻（接口给的是`北京时间`的墙上时间，不带时区）。
    pub date_time: NaiveDateTime,
    pub kind: BillKind,
    /// 正数，单位「分」。
    pub amount: AmountType,
    /// 店家/摘要，已归一化（去首尾空白、压缩空格）。
    pub memo: String,
    /// 备注里带的余额，拿不到就是 None。
    pub balance: Option<AmountType>,
    /// 接口给的类型名，如「网联协议支付」。
    pub raw_type: String,
    /// 去重指纹：同一笔流水必须每次算出来都一样。
    pub fingerprint: String,
}

impl BankTransaction {
    /// 用于界面显示：`2026-10-18 20:38 支出 14.20 网银在线-京东商城-正新鸡排`
    pub fn describe(&self) -> String {
        format!(
            "{} {} {} {}",
            self.date_time.format(TIMESTAMP_FORMAT),
            match self.kind {
                BillKind::Income => "收入",
                BillKind::Expense => "支出",
            },
            crate::common::money::format_amount(self.amount),
            self.memo
        )
    }

    /// 交易日（记账用）。
    pub fn date(&self) -> NaiveDate {
        self.date_time.date()
    }

    /// 记账用的时间戳。
    ///
    /// 银行给的是**北京时间的墙上时间**（不带时区），而账单库里存的是 UTC。
    /// 这里刻意把墙上时间当成 UTC 存，这样 `date_naive()` 得到的日期与银行
    /// 流水上的日期完全一致——否则凌晨的交易会因为减 8 小时跑到前一天去。
    pub fn as_utc(&self) -> DateTime<Utc> {
        Utc.from_utc_datetime(&self.date_time)
    }
}

/// 一次「同步」的结果：窗口 + 拉到的全部流水（还没去重、没入库）。
#[derive(Debug, Clone)]
pub struct SyncResult {
    pub window: SyncWindow,
    pub transactions: Vec<BankTransaction>,
}

/// 解析 `querytrans` 的返回体。
///
/// 兼容三种情况：正常返回、`result` 为空（没有交易）、业务码非 200（报错）。
/// 如果返回的是 HTML（登录页），会给出「会话可能已失效」的提示而不是 JSON 语法错误。
pub fn parse_querytrans_response(text: &str) -> Result<Vec<BankTransaction>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') {
        bail!(
            "返回的不是 JSON（前 60 个字符：{}），可能登录会话已失效，请重新登录后再试",
            trimmed.chars().take(60).collect::<String>()
        );
    }

    let root: serde_json::Value =
        serde_json::from_str(trimmed).context("解析银行返回的 JSON 失败")?;

    // 第一层：sysCode / sysMsg
    let sys_code = root
        .get("sysCode")
        .and_then(|value| value.as_i64())
        .unwrap_or(200);
    if sys_code != 200 {
        let message = root
            .get("sysMsg")
            .and_then(|value| value.as_str())
            .unwrap_or("未知错误");
        bail!("银行接口返回异常：sysCode={sys_code}，{message}");
    }

    // 第二层：bizResult.code / message
    let biz = root.get("bizResult").unwrap_or(&root);
    let biz_code = biz
        .get("code")
        .and_then(|value| value.as_str())
        .unwrap_or("200");
    if biz_code != "200" {
        let message = biz
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("未知错误");
        bail!("银行接口返回异常：code={biz_code}，{message}");
    }

    // 第三层：data.result[]，可能不存在（没有交易）
    let Some(rows) = biz
        .get("data")
        .and_then(|data| data.get("result"))
        .and_then(|result| result.as_array())
    else {
        return Ok(Vec::new());
    };

    let mut transactions = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        match parse_row(row) {
            Ok(transaction) => transactions.push(transaction),
            // 单条脏数据不该拖垮整批：跳过并留下位置信息
            Err(error) => eprintln!("跳过第 {} 条流水：{error:#}", index + 1),
        }
    }

    Ok(transactions)
}

/// 解析单条流水。
fn parse_row(row: &serde_json::Value) -> Result<BankTransaction> {
    let date_time_text = row
        .get("dateTime")
        .and_then(|value| value.as_str())
        .context("缺少 dateTime")?
        .trim();
    let date_time = NaiveDateTime::parse_from_str(date_time_text, TIMESTAMP_FORMAT)
        .with_context(|| format!("dateTime 格式不对：{date_time_text}"))?;

    // 金额优先用字符串 `money`（精确到分，没有浮点误差），拿不到再退回数值
    let money_text = row
        .get("money")
        .and_then(|value| value.as_str())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    let signed = match &money_text {
        Some(text) => parse_cents(text).with_context(|| format!("money 无法解析：{text}"))?,
        None => {
            let value = row
                .get("inOutMoney")
                .and_then(|value| value.as_f64())
                .context("既没有 money 也没有 inOutMoney")?;
            (value * 100.0).round() as AmountType
        }
    };

    if signed == 0 {
        bail!("金额为 0，不像一笔有效流水");
    }

    let kind = if signed > 0 {
        BillKind::Income
    } else {
        BillKind::Expense
    };
    let amount = signed.abs();

    let memo = row
        .get("memo")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let memo = normalize_memo(&memo);

    let balance = row
        .get("balance")
        .and_then(|value| value.as_str())
        .and_then(|text| parse_cents(text).ok());

    let raw_type = row
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();

    // 指纹用接口原始文本，避免归一化以后两笔不同流水撞在一起
    let fingerprint = format!("{date_time_text}|{}|{memo}", money_text.unwrap_or_default());

    Ok(BankTransaction {
        date_time,
        kind,
        amount,
        memo,
        balance,
        raw_type,
        fingerprint,
    })
}

/// 把 `"14.20"` / `"-14.20"` / `"+79.00"` / `"1,135.77"` 解析成「分」。
///
/// 全程走字符串，不经过浮点，避免 0.1+0.2 那类误差。
pub fn parse_cents(text: &str) -> Result<AmountType> {
    let cleaned: String = text
        .trim()
        .chars()
        .filter(|character| *character != ',' && !character.is_whitespace())
        .collect();

    if cleaned.is_empty() {
        bail!("空金额");
    }

    let (negative, digits) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };

    let (integer, fraction) = match digits.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (digits, ""),
    };

    if integer.is_empty() && fraction.is_empty() {
        bail!("金额里没有数字：{text}");
    }
    if !integer.chars().all(|character| character.is_ascii_digit())
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        bail!("金额里有非法字符：{text}");
    }
    // 只接受两位小数；多出来的位直接报错，免得悄悄四舍五入
    if fraction.len() > 2 {
        bail!("金额小数位超过两位：{text}");
    }

    let integer: i64 = if integer.is_empty() {
        0
    } else {
        integer.parse().with_context(|| format!("金额整数部分溢出：{text}"))?
    };

    let cents: i64 = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<i64>()? * 10,
        _ => fraction.parse::<i64>()?,
    };

    let total = integer
        .checked_mul(100)
        .and_then(|value| value.checked_add(cents))
        .with_context(|| format!("金额溢出：{text}"))?;

    Ok(if negative { -total } else { total })
}

/// 店家名归一化：去首尾空白、把连续空白压成一个空格。
pub fn normalize_memo(memo: &str) -> String {
    memo.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 已知的支付渠道前缀。同一个店家经由不同渠道进来，memo 前缀不同，
/// 记忆系统要靠它把它们认成同一个店家。
const CHANNEL_PREFIXES: [&str; 8] = [
    "网银在线-京东商城-",
    "财付通-微信支付-",
    "财付通-",
    "银联快捷支付-",
    "银联-",
    "支付宝-",
    "微信支付-",
    "云闪付-",
];

/// 记忆系统的「店家键」：在归一化基础上，再把渠道前缀一层层剥掉。
///
/// 例：`"财付通-微信支付-衢味辣哥俩土菜馆 "` → `"衢味辣哥俩土菜馆"`
pub fn memo_key(memo: &str) -> String {
    let mut current = normalize_memo(memo);

    loop {
        let stripped = CHANNEL_PREFIXES
            .iter()
            .find_map(|prefix| current.strip_prefix(prefix))
            .map(|rest| rest.trim_start().to_string());

        match stripped {
            Some(rest) if !rest.is_empty() => current = rest,
            _ => break,
        }
    }

    current
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    // ---------- 同步窗口 ----------

    #[test]
    fn today_window_is_a_single_day() {
        let window = SyncWindow::today(date(2026, 10, 18));

        assert_eq!(window.start, date(2026, 10, 18));
        assert_eq!(window.end, date(2026, 10, 18));
        assert_eq!(window.start_text(), "20261018");
        assert_eq!(window.end_text(), "20261018");
        assert_eq!(window.days(), 1);
        assert_eq!(window.label, "今日");
    }

    #[test]
    fn week_window_runs_monday_to_today() {
        // 2026-10-18 是周日，本周一是 10-12
        let window = SyncWindow::this_week(date(2026, 10, 18), None).expect("应有窗口");

        assert_eq!(window.start, date(2026, 10, 12), "周一为起点");
        assert_eq!(window.end, date(2026, 10, 18), "周日为终点");
        assert_eq!(window.describe(), "20261012–20261018");
        assert_eq!(window.days(), 7);
    }

    #[test]
    fn week_window_never_looks_into_the_future() {
        // 2026-10-14 是周三：窗口应止于当天，而不是周日
        let window = SyncWindow::this_week(date(2026, 10, 14), None).expect("应有窗口");

        assert_eq!(window.start, date(2026, 10, 12));
        assert_eq!(window.end, date(2026, 10, 14), "未来的日子不看");
    }

    #[test]
    fn week_window_starts_after_the_latest_local_bill() {
        // 本地已经记到 10-15，那本次应从 10-16 开始
        let window = SyncWindow::this_week(date(2026, 10, 18), Some(date(2026, 10, 15)))
            .expect("应有窗口");

        assert_eq!(window.start, date(2026, 10, 16));
        assert_eq!(window.end, date(2026, 10, 18));
    }

    #[test]
    fn week_window_is_clamped_to_monday() {
        // 本地最新账单在上周，起点仍然不早于本周一
        let window = SyncWindow::this_week(date(2026, 10, 14), Some(date(2026, 10, 1)))
            .expect("应有窗口");

        assert_eq!(window.start, date(2026, 10, 12), "不能跨出本周");
    }

    #[test]
    fn week_window_disappears_when_local_is_already_up_to_date() {
        // 本地已经记到今天，就没有需要同步的区间
        assert_eq!(
            SyncWindow::this_week(date(2026, 10, 18), Some(date(2026, 10, 18))),
            None
        );
        // 记到未来也一样
        assert_eq!(
            SyncWindow::this_week(date(2026, 10, 18), Some(date(2026, 10, 25))),
            None
        );
    }

    // ---------- 金额解析 ----------

    #[test]
    fn parses_money_strings_without_floats() {
        assert_eq!(parse_cents("14.20").unwrap(), 1420);
        assert_eq!(parse_cents("-14.20").unwrap(), -1420);
        assert_eq!(parse_cents("+79.00").unwrap(), 7900);
        assert_eq!(parse_cents("11,135.77").unwrap(), 1_113_577);
        assert_eq!(parse_cents("0.05").unwrap(), 5);
        assert_eq!(parse_cents("5").unwrap(), 500);
        assert_eq!(parse_cents(".5").unwrap(), 50);
        assert_eq!(parse_cents(" 616.25 ").unwrap(), 61_625);
    }

    #[test]
    fn rejects_malformed_money() {
        assert!(parse_cents("").is_err());
        assert!(parse_cents("abc").is_err());
        assert!(parse_cents("1.234").is_err(), "超过两位小数应报错，不静默四舍五入");
        assert!(parse_cents("1,2x").is_err());
    }

    // ---------- 店家名归一化 ----------

    #[test]
    fn normalizes_and_strips_channel_prefixes() {
        assert_eq!(
            memo_key("财付通-微信支付-衢味辣哥俩土菜馆 "),
            "衢味辣哥俩土菜馆"
        );
        assert_eq!(memo_key("网银在线-京东商城-正新鸡排 "), "正新鸡排");
        // 重复前缀要一层层剥
        assert_eq!(
            memo_key("支付宝-支付宝-消费-友宝智能货柜 "),
            "消费-友宝智能货柜"
        );
        // 没有前缀的原样返回
        assert_eq!(memo_key("  江南体育中心   自助售货机 "), "江南体育中心 自助售货机");
        // 只有前缀也不能剥成空
        assert_eq!(memo_key("支付宝-"), "支付宝-");
    }

    #[test]
    fn bank_date_survives_the_utc_conversion() {
        // 凌晨 01:18 的交易：如果按真实时区换算（-8h）会掉到前一天，
        // 这里必须仍落在银行给的那一天
        let txn = BankTransaction {
            date_time: date(2026, 10, 16).and_hms_opt(1, 18, 48).unwrap(),
            kind: BillKind::Expense,
            amount: 3400,
            memo: "Apple".to_string(),
            balance: None,
            raw_type: String::new(),
            fingerprint: "fp".to_string(),
        };

        assert_eq!(txn.as_utc().date_naive(), date(2026, 10, 16));
        // 墙上时间原样保留（晚上 01:18 仍显示 01:18）
        assert_eq!(
            txn.as_utc().format("%H:%M:%S").to_string(),
            "01:18:48"
        );
    }

    // ---------- 响应解析 ----------

    /// 用需求里给的真实返回体（保留结构，砍到 3 条）。
    const SAMPLE: &str = r#"
    {
      "sysCode": 200,
      "sysMsg": "Success",
      "sysResult": {},
      "bizResult": {
        "code": "200",
        "message": "",
        "data": {
          "result": [
            {
              "index": 24,
              "dateTime": "2026-10-18 20:38:03",
              "date": "2026-10-18",
              "time": "20:38:03",
              "money": "-14.20",
              "inOutMoney": -14.20,
              "inMoney": "",
              "outMoney": "14.20",
              "balance": "11,135.77",
              "balanceMoney": 11135.77,
              "type": "网联协议支付",
              "typeId": "N5CP",
              "memo": "网银在线-京东商城-正新鸡排 "
            },
            {
              "index": 9,
              "dateTime": "2026-10-14 16:52:06",
              "money": "+79.00",
              "inOutMoney": 79.00,
              "inMoney": "79.00",
              "outMoney": "",
              "balance": "11,402.50",
              "type": "网联退款",
              "typeId": "N5CR",
              "memo": "支付宝-泉州屿晏贸易有限公司 "
            },
            {
              "index": 8,
              "dateTime": "2026-10-14 12:54:32",
              "money": "-89.00",
              "inOutMoney": -89.00,
              "balance": "11,323.50",
              "type": "网联协议支付",
              "memo": "支付宝-上海世瑾贸易有限公司 "
            }
          ],
          "typeList": [{ "type": "[ 全部 ]", "typeId": "-" }]
        }
      }
    }
    "#;

    #[test]
    fn parses_the_sample_response() {
        let list = parse_querytrans_response(SAMPLE).expect("应能解析");

        assert_eq!(list.len(), 3);

        let first = &list[0];
        assert_eq!(first.kind, BillKind::Expense);
        assert_eq!(first.amount, 1420, "支出金额取绝对值");
        assert_eq!(first.memo, "网银在线-京东商城-正新鸡排", "memo 应去掉尾部空格");
        assert_eq!(first.balance, Some(1_113_577));
        assert_eq!(first.raw_type, "网联协议支付");
        assert_eq!(
            first.date_time,
            NaiveDate::from_ymd_opt(2026, 10, 18)
                .unwrap()
                .and_hms_opt(20, 38, 3)
                .unwrap()
        );
        assert_eq!(first.date(), date(2026, 10, 18));

        // 收入方向由正负号决定
        let refund = &list[1];
        assert_eq!(refund.kind, BillKind::Income);
        assert_eq!(refund.amount, 7900);
        assert_eq!(refund.raw_type, "网联退款");
    }

    #[test]
    fn fingerprints_are_stable_and_unique_per_transaction() {
        let first = parse_querytrans_response(SAMPLE).unwrap();
        let second = parse_querytrans_response(SAMPLE).unwrap();

        // 同一份数据解析两次，指纹必须完全一样（否则去重会失效）
        let fingerprints: Vec<&String> = first.iter().map(|txn| &txn.fingerprint).collect();
        let again: Vec<&String> = second.iter().map(|txn| &txn.fingerprint).collect();
        assert_eq!(fingerprints, again);

        // 三笔流水指纹互不相同
        let unique: std::collections::HashSet<&String> = first.iter().map(|txn| &txn.fingerprint).collect();
        assert_eq!(unique.len(), 3);
        assert!(first[0].fingerprint.contains("2026-10-18 20:38:03"));
    }

    #[test]
    fn empty_result_is_ok() {
        let text = r#"{"sysCode":200,"bizResult":{"code":"200","data":{"result":[]}}}"#;
        assert!(parse_querytrans_response(text).unwrap().is_empty());

        // 连 data 都没有也算「没有交易」
        let text = r#"{"sysCode":200,"bizResult":{"code":"200"}}"#;
        assert!(parse_querytrans_response(text).unwrap().is_empty());
    }

    #[test]
    fn business_error_is_surfaced() {
        let text = r#"{"sysCode":200,"bizResult":{"code":"500","message":"会话已失效"}}"#;
        let error = parse_querytrans_response(text).expect_err("应报错");
        assert!(error.to_string().contains("会话已失效"), "{error}");
    }

    #[test]
    fn sys_error_is_surfaced() {
        let text = r#"{"sysCode":500,"sysMsg":"系统繁忙"}"#;
        let error = parse_querytrans_response(text).expect_err("应报错");
        assert!(error.to_string().contains("系统繁忙"), "{error}");
    }

    #[test]
    fn html_login_page_gives_a_friendly_error() {
        let error = parse_querytrans_response("<html><body>请登录</body></html>")
            .expect_err("HTML 应报错");
        assert!(
            error.to_string().contains("会话已失效"),
            "应提示重新登录而不是 JSON 语法错误：{error}"
        );
    }

    #[test]
    fn bad_row_is_skipped_without_losing_the_batch() {
        // 中间一条 dateTime 坏了：那一条跳过，其余保留
        let text = r#"{
          "sysCode":200,
          "bizResult":{"code":"200","data":{"result":[
            {"dateTime":"2026-10-18 20:38:03","money":"-14.20","memo":"好的一条"},
            {"dateTime":"不是时间","money":"-1.00","memo":"坏的一条"},
            {"dateTime":"2026-10-17 10:00:00","money":"-2.00","memo":"另一条"}
          ]}}
        }"#;

        let list = parse_querytrans_response(text).expect("整批不应失败");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].memo, "好的一条");
        assert_eq!(list[1].memo, "另一条");
    }

    #[test]
    fn zero_amount_rows_are_rejected() {
        let text = r#"{
          "sysCode":200,
          "bizResult":{"code":"200","data":{"result":[
            {"dateTime":"2026-10-18 20:38:03","money":"0.00","memo":"零元"}
          ]}}
        }"#;
        assert!(parse_querytrans_response(text).unwrap().is_empty());
    }
}
