//! 应用级键值状态。
//!
//! 用于跨会话记住一些「上次用了什么」的小状态（目前是新增账单时上次使用的日期）。
//! 存数据库而不是本地文件：换机器、重启都还能沿用。

use anyhow::Result;
use sqlx::PgPool;

#[derive(Clone)]
pub struct StateStore {
    pool: PgPool,
}

impl StateStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM app_state WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(value,)| value))
    }

    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO app_state (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::db;

    /// 需要真实数据库：只有设置 BILLER_TEST_DATABASE_URL 时才会执行。
    #[tokio::test]
    async fn round_trips_values() {
        let Ok(url) = std::env::var("BILLER_TEST_DATABASE_URL") else {
            return;
        };
        let pool = db::connect(&url).await.expect("连接测试库");
        db::init_schema(&pool).await.expect("初始化表结构");
        let store = StateStore::new(pool);

        store.set("test.key", "hello").await.expect("写入");
        assert_eq!(
            store.get("test.key").await.expect("读取"),
            Some("hello".to_string())
        );

        store.set("test.key", "world").await.expect("覆盖");
        assert_eq!(
            store.get("test.key").await.expect("读取"),
            Some("world".to_string())
        );

        assert_eq!(store.get("test.missing").await.expect("读取"), None);
    }
}
