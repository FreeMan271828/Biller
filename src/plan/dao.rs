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
        // 三个键必须一起生效：中途失败要整体回滚，
        // 否则会留下「预计金额是新的、模式还是旧的」这种自相矛盾的计划。
        let mut tx = self.pool.any().begin().await?;

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
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::db;

    use crate::plan::model::PlanMode;

    /// 临时 SQLite 文件 + 建表；返回池与目录（测试结束删目录）。
    async fn setup() -> (Pool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "biller-plan-dao-{}-{:?}",
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

    #[tokio::test]
    async fn saves_and_reloads_the_whole_plan() {
        let (pool, dir) = setup().await;
        let dao = PlanDao::new(pool.clone());

        // 空库：全是默认值
        assert_eq!(dao.get().await.unwrap(), SpendingPlan::default());

        let plan = SpendingPlan {
            expected: 12_000,
            savings: 8_000,
            mode: PlanMode::DailyOnly,
        };
        dao.save(&plan).await.unwrap();
        assert_eq!(dao.get().await.unwrap(), plan, "三个字段都应落库");

        // 覆盖写：update 路径
        let updated = SpendingPlan {
            expected: 15_000,
            savings: 0,
            mode: PlanMode::Cumulative,
        };
        dao.save(&updated).await.unwrap();
        assert_eq!(dao.get().await.unwrap(), updated);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
