//! 开销计划的持久化：复用 `app_state` 键值表，不额外建表。

use anyhow::Result;

use crate::common::db::Pool;
use crate::plan::model::{PlanMode, SpendingPlan};

/// 计划在 `app_state` 里的键名。
pub const KEY_EXPECTED: &str = "plan.expected";
pub const KEY_SAVINGS: &str = "plan.savings";
pub const KEY_MODE: &str = "plan.mode";

#[derive(Clone)]
pub struct PlanDao {
    pool: Pool,
}

impl PlanDao {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    pub async fn get(&self) -> Result<SpendingPlan> {
        // `key = ANY(?)` 在 sqlx::Any 下绑不了数组（Vec<String> 没有 Encode 实现），
        // 改成 `IN` 三个占位符 —— PostgreSQL 同样认这种写法。
        let rows: Vec<(String, String)> =
            sqlx::query_as(&self.pool.shared(
                "SELECT key, value FROM app_state WHERE key IN (?, ?, ?)",
            ))
            .bind(KEY_EXPECTED)
                .bind(KEY_SAVINGS)
                .bind(KEY_MODE)
                .fetch_all(self.pool.any())
                .await?;

        let mut plan = SpendingPlan::default();
        for (key, value) in rows {
            // 金额值存的是「分」的十进制字符串；脏数据当作 0/默认值，不要让界面崩
            match key.as_str() {
                KEY_EXPECTED => plan.expected = value.parse().unwrap_or(0),
                KEY_SAVINGS => plan.savings = value.parse().unwrap_or(0),
                KEY_MODE => plan.mode = PlanMode::parse(&value).unwrap_or_default(),
                _ => {}
            }
        }

        Ok(plan)
    }

    pub async fn save(&self, plan: &SpendingPlan) -> Result<()> {
        for (key, value) in [
            (KEY_EXPECTED, plan.expected.to_string()),
            (KEY_SAVINGS, plan.savings.to_string()),
            (KEY_MODE, plan.mode.as_str().to_string()),
        ] {
            sqlx::query(&self.pool.shared(
                "INSERT INTO app_state (key, value) VALUES (?, ?)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            ))
            .bind(key)
            .bind(value)
            .execute(self.pool.any())
            .await?;
        }

        Ok(())
    }
}
