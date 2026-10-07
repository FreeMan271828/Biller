//! 月度起始金额的数据访问层：只写 SQL，不做业务判断。

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::PgPool;

use crate::common::types::AmountType;

#[derive(Clone)]
pub struct MonthDao {
    pool: PgPool,
}

impl MonthDao {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 该月起始金额；未设置过返回 0。
    pub async fn start_balance(&self, month: NaiveDate) -> Result<AmountType> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT start_balance FROM month_balances WHERE month = $1")
                .bind(month)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(value,)| value).unwrap_or(0))
    }

    /// 写入或覆盖该月起始金额。
    pub async fn set_start_balance(&self, month: NaiveDate, amount: AmountType) -> Result<()> {
        sqlx::query(
            "INSERT INTO month_balances (month, start_balance) VALUES ($1, $2)
             ON CONFLICT (month) DO UPDATE SET start_balance = EXCLUDED.start_balance",
        )
        .bind(month)
        .bind(amount)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
