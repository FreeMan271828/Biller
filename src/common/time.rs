//! 月份 / 时间解析与格式化。
//!
//! 月度统计统一按 **UTC** 归属（与 `BillEntity::created_at` 的类型一致），
//! 月份用「当月 1 号」的 `NaiveDate` 表示。

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// 取某天所在月份的 1 号。
pub fn first_day_of_month(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).expect("月份一定合法")
}

/// 解析月份，支持 `2026-10`、`2026/10`、`2026-10-05`，统一返回当月 1 号。
pub fn parse_month(input: &str) -> Result<NaiveDate> {
    let trimmed = input.trim();
    let parts: Vec<&str> = trimmed.split(['-', '/']).collect();
    let (year, month) = match parts.as_slice() {
        [year, month] => (*year, *month),
        [year, month, _day] => (*year, *month),
        _ => bail!("月份格式应为 YYYY-MM（也可写 YYYY-MM-DD），收到：{input}"),
    };

    let year: i32 = year
        .parse()
        .map_err(|_| anyhow!("月份里的年份不是数字：{input}"))?;
    let month: u32 = month
        .parse()
        .map_err(|_| anyhow!("月份里的月份不是数字：{input}"))?;

    NaiveDate::from_ymd_opt(year, month, 1).ok_or_else(|| anyhow!("不存在的月份：{input}"))
}

/// 把月份格式化成 `YYYY-MM`。
pub fn format_month(month: NaiveDate) -> String {
    format!("{:04}-{:02}", month.year(), month.month())
}

/// 只到天：`YYYY-MM-DD`（账单列表里时间只展示到天）。
pub fn format_date(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d").to_string()
}

/// 把时间格式化成 `YYYY-MM-DD HH:MM:SS`（UTC）。
pub fn format_datetime(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 解析时间；传 `None` 时取当前时间。
///
/// 支持 `2026-10-05`、`2026-10-05 13:20`、`2026-10-05 13:20:00`、
/// `2026-10-05T13:20:00`、`2026-10-05T13:20:00Z`。
/// 不带时区的写法一律按 **UTC** 解释。
pub fn parse_datetime(input: Option<&str>) -> Result<DateTime<Utc>> {
    let Some(raw) = input else {
        return Ok(Utc::now());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Utc::now());
    }

    if let Ok(value) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(value.with_timezone(&Utc));
    }

    for format in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, format) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }

    for format in ["%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(date) = NaiveDate::parse_from_str(trimmed, format) {
            let naive = date.and_hms_opt(0, 0, 0).expect("0 点一定合法");
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }

    bail!("时间格式无法识别：{raw}（示例：2026-10-05、\"2026-10-05 13:20:00\"、2026-10-05T13:20:00Z）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_months() {
        let expected = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert_eq!(parse_month("2026-10").unwrap(), expected);
        assert_eq!(parse_month("2026/10").unwrap(), expected);
        assert_eq!(parse_month("2026-10-05").unwrap(), expected);
        assert!(parse_month("2026-13").is_err());
        assert!(parse_month("abc").is_err());
    }

    #[test]
    fn parses_datetimes() {
        let date_only = parse_datetime(Some("2026-10-05")).unwrap();
        assert_eq!(date_only.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-10-05 00:00:00");

        let with_time = parse_datetime(Some("2026-10-05 13:20:00")).unwrap();
        assert_eq!(with_time.format("%H:%M:%S").to_string(), "13:20:00");

        let rfc = parse_datetime(Some("2026-10-05T13:20:00Z")).unwrap();
        assert_eq!(rfc, with_time);

        assert!(parse_datetime(Some("昨天")).is_err());
    }

    #[test]
    fn formats_date_only() {
        let value = parse_datetime(Some("2026-10-05 12:30:00")).unwrap();
        assert_eq!(format_date(value), "2026-10-05");
        assert_eq!(format_datetime(value), "2026-10-05 12:30:00");
    }
}
