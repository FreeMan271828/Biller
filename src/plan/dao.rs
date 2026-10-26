//! 资金计划的持久化：复用 `app_state` 键值表，不额外建表。
//!
//! 整份计划（若干块）存成一行 JSON：块的数量是可变的，拆成定长列反而别扭。
//! 老版本用 `plan.expected` / `plan.savings` 两个标量键，读取时自动迁移成两块，
//! 写回时再把旧键删掉——同一份计划不留两个来源。

use anyhow::Result;

use crate::common::db::Pool;
use crate::common::types::AmountType;
use crate::plan::model::{PlanItem, SpendingPlan};

/// 计划本体在 `app_state` 里的键名。
pub const KEY_ITEMS: &str = "plan.items";
/// 旧版标量键：只读不写，用于迁移。
pub const KEY_EXPECTED: &str = "plan.expected";
/// 旧版标量键：只读不写，用于迁移。
pub const KEY_SAVINGS: &str = "plan.savings";

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
        let rows: Vec<(String, String)> = sqlx::query_as(
            &self
                .pool
                .shared("SELECT key, value FROM app_state WHERE key IN (?, ?, ?)"),
        )
        .bind(KEY_ITEMS)
        .bind(KEY_EXPECTED)
        .bind(KEY_SAVINGS)
        .fetch_all(self.pool.any())
        .await?;

        let mut encoded: Option<String> = None;
        let mut expected: Option<AmountType> = None;
        let mut savings: Option<AmountType> = None;

        for (key, value) in rows {
            match key.as_str() {
                KEY_ITEMS => encoded = Some(value),
                // 脏数据当作缺省，不要让界面崩
                KEY_EXPECTED => expected = value.parse().ok(),
                KEY_SAVINGS => savings = value.parse().ok(),
                _ => {}
            }
        }

        if let Some(raw) = encoded {
            if let Some(plan) = decode(&raw) {
                return Ok(plan);
            }
        }

        Ok(migrate(expected.unwrap_or(0), savings.unwrap_or(0)))
    }

    pub async fn save(&self, plan: &SpendingPlan) -> Result<()> {
        let value = encode(plan);

        // 写新键 + 清旧键必须一起生效：中途失败会留下「新计划没有、旧计划还在」
        let mut tx = self.pool.any().begin().await?;

        sqlx::query(&self.pool.shared(
            "INSERT INTO app_state (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        ))
        .bind(KEY_ITEMS)
        .bind(value)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            &self
                .pool
                .shared("DELETE FROM app_state WHERE key IN (?, ?)"),
        )
        .bind(KEY_EXPECTED)
        .bind(KEY_SAVINGS)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }
}

/// 计划 → JSON 数组。
///
/// 新格式每条是对象 `{"name","amount","category"?}`：分类是可选的，
/// 不绑定就不写这个键（省得一串 `null` 到处飞）。
/// Cargo.toml 里只有 `serde_json`、没有 serde 的 derive 宏，
/// 所以这里手工构造 `Value`，不为了一个字段多引一个依赖。
fn encode(plan: &SpendingPlan) -> String {
    let items: Vec<serde_json::Value> = plan
        .items()
        .iter()
        .map(|item| {
            let mut object = serde_json::Map::new();
            object.insert(
                "name".to_string(),
                serde_json::Value::String(item.name.clone()),
            );
            object.insert(
                "amount".to_string(),
                serde_json::Value::Number(item.amount.into()),
            );
            if let Some(category) = &item.category {
                object.insert(
                    "category".to_string(),
                    serde_json::Value::String(category.clone()),
                );
            }
            serde_json::Value::Object(object)
        })
        .collect();

    serde_json::to_string(&serde_json::Value::Array(items))
        .unwrap_or_else(|_| "[]".to_string())
}

/// JSON → 计划；格式不认识就当没存过，让上层退回默认值。
///
/// 两种历史格式都认：
///
/// * 1.2.0 的 `[[name, amount], ...]`（二元组数组，没有分类）
/// * 现在的 `[{name, amount, category?}, ...]`
fn decode(raw: &str) -> Option<SpendingPlan> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let array = value.as_array()?;
    let mut items = Vec::with_capacity(array.len());

    for entry in array {
        match entry {
            // 新格式：对象，分类可选。
            serde_json::Value::Object(object) => {
                let name = object.get("name")?.as_str()?.to_string();
                let amount = object.get("amount")?.as_i64()?;
                let category = object
                    .get("category")
                    .and_then(|category| category.as_str())
                    .map(str::to_string);
                items.push(PlanItem::new(name, amount).with_category(category.as_deref()));
            }
            // 旧格式：二元组数组。
            serde_json::Value::Array(pair) if pair.len() == 2 => {
                let name = pair[0].as_str()?.to_string();
                let amount = pair[1].as_i64()?;
                items.push(PlanItem::new(name, amount));
            }
            _ => return None,
        }
    }

    Some(SpendingPlan::new(items))
}

/// 旧版「预计 / 攒钱」→ 新的分块计划。
///
/// 旧语义里 `预计 - 攒钱` 是当月可花的钱，攒钱那部分不花：
/// 于是落到两块——「日常开销」拿可花的那部分，「存款」拿攒下的那部分。
fn migrate(expected: AmountType, savings: AmountType) -> SpendingPlan {
    if expected <= 0 && savings <= 0 {
        return SpendingPlan::default();
    }

    let spendable = (expected - savings).max(0);
    let mut items = Vec::new();
    if spendable > 0 {
        items.push(PlanItem::new("日常开销", spendable));
    }
    if savings > 0 {
        items.push(PlanItem::new("存款", savings));
    }

    SpendingPlan::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::db;

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

    fn plan_of(pairs: &[(&str, AmountType)]) -> SpendingPlan {
        SpendingPlan::new(
            pairs
                .iter()
                .map(|(name, amount)| PlanItem::new(*name, *amount))
                .collect(),
        )
    }

    #[tokio::test]
    async fn saves_and_reloads_every_bucket() {
        let (pool, dir) = setup().await;
        let dao = PlanDao::new(pool.clone());

        // 空库：空计划
        assert_eq!(dao.get().await.unwrap(), SpendingPlan::default());

        let plan = plan_of(&[("日常开销", 3_000_00), ("存款", 9_000_00), ("VPN", 50_00)]);
        dao.save(&plan).await.unwrap();
        assert_eq!(dao.get().await.unwrap(), plan, "每一块都应落库");

        // 覆盖写：块数变少也要正确覆盖，不能残留旧块
        let updated = plan_of(&[("存款", 12_000_00)]);
        dao.save(&updated).await.unwrap();
        assert_eq!(dao.get().await.unwrap(), updated);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn legacy_scalar_plan_is_migrated_into_buckets() {
        let (pool, dir) = setup().await;
        let dao = PlanDao::new(pool.clone());

        for (key, value) in [(KEY_EXPECTED, "1200000"), (KEY_SAVINGS, "800000")] {
            sqlx::query(&pool.shared(
                "INSERT INTO app_state (key, value) VALUES (?, ?)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            ))
            .bind(key)
            .bind(value)
            .execute(pool.any())
            .await
            .unwrap();
        }

        let plan = dao.get().await.unwrap();
        assert_eq!(plan.items().len(), 2);
        assert_eq!(plan.items()[0].name, "日常开销");
        assert_eq!(plan.items()[0].amount, 4_000_00);
        assert_eq!(plan.items()[1].name, "存款");
        assert_eq!(plan.items()[1].amount, 8_000_00);
        assert_eq!(plan.total(), 12_000_00);

        // 保存后旧键必须消失，避免两个来源打架
        dao.save(&plan).await.unwrap();
        let rows: Vec<(String,)> =
            sqlx::query_as(&pool.shared("SELECT key FROM app_state WHERE key = ?"))
                .bind(KEY_EXPECTED)
                .fetch_all(pool.any())
                .await
                .unwrap();
        assert!(rows.is_empty(), "迁移完要清掉旧键");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn broken_payload_falls_back_to_the_legacy_default() {
        let (pool, dir) = setup().await;
        let dao = PlanDao::new(pool.clone());

        sqlx::query(&pool.shared(
            "INSERT INTO app_state (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        ))
        .bind(KEY_ITEMS)
        .bind("这不是 JSON")
        .execute(pool.any())
        .await
        .unwrap();

        assert_eq!(dao.get().await.unwrap(), SpendingPlan::default());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_round_trips_through_json() {
        let plan = plan_of(&[("房租", 2_500_00), ("含\"引号\"", 100)]);
        let decoded = decode(&encode(&plan)).expect("编码是可逆的");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn category_binding_survives_the_round_trip() {
        let plan = SpendingPlan::new(vec![
            PlanItem::new("吃饭", 1_500_00).with_category(Some("餐饮")),
            PlanItem::new("存款", 9_000_00),
        ]);

        let decoded = decode(&encode(&plan)).expect("编码是可逆的");
        assert_eq!(decoded, plan, "分类绑定要能落库再读回");
        assert_eq!(decoded.items()[0].category.as_deref(), Some("餐饮"));
        assert_eq!(decoded.items()[1].category, None);
    }

    #[test]
    fn old_pair_arrays_are_still_readable() {
        // 1.2.0 落库的样子：没有分类字段，读回来必须变成「不绑定」。
        let legacy = "[[\"日常开销\", 300000], [\"存款\", 900000]]";
        let decoded = decode(legacy).expect("旧格式要认");
        assert_eq!(decoded.items().len(), 2);
        assert_eq!(decoded.items()[0].name, "日常开销");
        assert_eq!(decoded.items()[0].amount, 300_000);
        assert_eq!(decoded.items()[0].category, None);
        assert_eq!(decoded.total(), 1_200_000);
    }
}
