//! 月度起始金额的数据访问层：只写 SQL，不做业务判断。

use anyhow::Result;
use chrono::NaiveDate;

use crate::common::db::Pool;
use crate::common::types::AmountType;

/// 月份以 ISO 文本（`YYYY-MM-DD`）绑定 —— `sqlx::Any` 不认 chrono 类型。
///
/// PostgreSQL 侧靠 `?::date` 转回来，SQLite 侧列本来就是 TEXT，直接比即可。
///
/// 注意：一律归一到「当月 1 号」。`month_balances.month` 的主键语义就是 1 号，
/// 传「当月 15 号」在 SQLite 里会写成一条永远查不到的 `2026-10-15` 记录。
fn month_text(month: NaiveDate) -> String {
    crate::common::time::first_day_of_month(month).to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::db;

    async fn setup() -> (Pool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "biller-month-dao-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let pool = db::connect(&db::sqlite_url(&dir.join("biller.db")))
            .await
            .expect("连接临时 SQLite");
        db::init_schema(&pool).await.expect("建表");

        (pool, dir)
    }

    /// 月份一律按「当月 1 号」落库：传 15 号也只能产生 `YYYY-MM-01` 这一条。
    ///
    /// PG 侧是 `?::date` 精确比较，不归一会读写两空；SQLite 侧则会留下
    /// 一条永远查不到的 `YYYY-MM-15`，两端口径必须一致。
    #[tokio::test]
    async fn normalizes_month_to_the_first_day() {
        let (pool, dir) = setup().await;
        let dao = MonthDao::new(pool.clone());

        let fifteenth = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        dao.set_start_balance(fifteenth, 12_345).await.unwrap();

        assert_eq!(
            dao.start_balance(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap())
                .await
                .unwrap(),
            12_345,
            "按 1 号读回必须命中"
        );

        let (stored_month,): (String,) = sqlx::query_as(
            &pool.shared("SELECT month FROM month_balances WHERE start_balance = ?"),
        )
        .bind(12_345_i64)
        .fetch_one(pool.any())
        .await
        .unwrap();
        assert_eq!(stored_month, "2026-10-01", "库里只能存 1 号");

        // 覆盖同一个月不应多出一行
        dao.set_start_balance(fifteenth, 999).await.unwrap();
        let (count,): (i64,) = sqlx::query_as(&pool.shared("SELECT COUNT(*) FROM month_balances"))
            .fetch_one(pool.any())
            .await
            .unwrap();
        assert_eq!(count, 1, "同一个月只应有一行");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
