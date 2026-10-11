//! 月度起始金额的数据访问层：只写 SQL，不做业务判断。

use anyhow::Result;
use chrono::NaiveDate;

use crate::common::db::Pool;
use crate::common::types::AmountType;

/// 月份以 ISO 文本（`YYYY-MM-DD`）绑定 —— `sqlx::Any` 不认 chrono 类型。
///
/// PostgreSQL 侧靠 `?::date` 转回来，SQLite 侧列本来就是 TEXT，直接比即可。
fn month_text(month: NaiveDate) -> String {
    month.to_string()
}

#[derive(Clone)]
pub struct MonthDao {
    pool: Pool,
}

impl MonthDao {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// 该月起始金额；未设置过返回 0。
    pub async fn start_balance(&self, month: NaiveDate) -> Result<AmountType> {
        let sql = self.pool.sql(
            "SELECT start_balance FROM month_balances WHERE month = ?::date",
            "SELECT start_balance FROM month_balances WHERE month = ?",
        );

        let row: Option<(i64,)> = sqlx::query_as(&sql)
            .bind(month_text(month))
            .fetch_optional(self.pool.any())
            .await?;

        Ok(row.map(|(value,)| value).unwrap_or(0))
    }

    /// 写入或覆盖该月起始金额。
    pub async fn set_start_balance(&self, month: NaiveDate, amount: AmountType) -> Result<()> {
        let sql = self.pool.sql(
            "INSERT INTO month_balances (month, start_balance) VALUES (?::date, ?)
             ON CONFLICT (month) DO UPDATE SET start_balance = EXCLUDED.start_balance",
            "INSERT INTO month_balances (month, start_balance) VALUES (?, ?)
             ON CONFLICT (month) DO UPDATE SET start_balance = EXCLUDED.start_balance",
        );

        sqlx::query(&sql)
            .bind(month_text(month))
            .bind(amount)
            .execute(self.pool.any())
            .await?;

        Ok(())
    }
}
