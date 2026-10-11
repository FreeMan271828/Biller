//! 分类的数据访问层：只写 SQL，不做业务判断。

use anyhow::Result;

use crate::category::model::Category;
use crate::common::db::Pool;
use crate::common::types::CategoryId;

#[derive(sqlx::FromRow)]
struct CategoryRow {
    id: i64,
    parent_id: Option<i64>,
    name: String,
}

impl From<CategoryRow> for Category {
    fn from(row: CategoryRow) -> Self {
        Self {
            id: row.id as CategoryId,
            parent_id: row.parent_id.map(|id| id as CategoryId),
            name: row.name,
        }
    }
}

#[derive(Clone)]
pub struct CategoryDao {
    pool: Pool,
}

impl CategoryDao {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, parent_id: Option<CategoryId>, name: &str) -> Result<CategoryId> {
        let (id,): (i64,) = sqlx::query_as(&self.pool.shared(
            "INSERT INTO categories (parent_id, name) VALUES (?, ?) RETURNING id",
        ))
        .bind(parent_id.map(|id| id as i64))
        .bind(name)
        .fetch_one(self.pool.any())
        .await?;
        Ok(id as CategoryId)
    }

    /// 顶层在前，同层按 id。
    pub async fn list(&self) -> Result<Vec<Category>> {
        let rows = sqlx::query_as::<_, CategoryRow>(
            "SELECT id, parent_id, name FROM categories
             ORDER BY parent_id NULLS FIRST, id",
        )
        .fetch_all(self.pool.any())
        .await?;
        Ok(rows.into_iter().map(Category::from).collect())
    }

    pub async fn get(&self, id: CategoryId) -> Result<Option<Category>> {
        let row = sqlx::query_as::<_, CategoryRow>(&self.pool.shared(
            "SELECT id, parent_id, name FROM categories WHERE id = ?",
        ))
                .bind(id as i64)
                .fetch_optional(self.pool.any())
                .await?;
        Ok(row.map(Category::from))
    }

    /// 按名称查找（不限层级），可能返回多个同名分类。
    pub async fn list_by_name(&self, name: &str) -> Result<Vec<Category>> {
        let rows = sqlx::query_as::<_, CategoryRow>(&self.pool.shared(
            "SELECT id, parent_id, name FROM categories WHERE name = ? ORDER BY id",
        ))
        .bind(name)
        .fetch_all(self.pool.any())
        .await?;
        Ok(rows.into_iter().map(Category::from).collect())
    }

    /// 同一父分类下的同名分类。
    ///
    /// `IS NOT DISTINCT FROM` 只有 PostgreSQL 认；SQLite 的 `IS` 就是同样的语义。
    pub async fn find_sibling(
        &self,
        parent_id: Option<CategoryId>,
        name: &str,
    ) -> Result<Option<Category>> {
        let sql = self.pool.sql(
            "SELECT id, parent_id, name FROM categories
             WHERE name = ? AND parent_id IS NOT DISTINCT FROM ?",
            "SELECT id, parent_id, name FROM categories
             WHERE name = ? AND parent_id IS ?",
        );

        let row = sqlx::query_as::<_, CategoryRow>(&sql)
            .bind(name)
            .bind(parent_id.map(|id| id as i64))
            .fetch_optional(self.pool.any())
            .await?;
        Ok(row.map(Category::from))
    }

    pub async fn count_children(&self, id: CategoryId) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as(&self.pool.shared("SELECT COUNT(*) FROM categories WHERE parent_id = ?"))
                .bind(id as i64)
                .fetch_one(self.pool.any())
                .await?;
        Ok(count)
    }

    /// 修改名称与父分类；返回受影响行数。
    pub async fn update(
        &self,
        id: CategoryId,
        parent_id: Option<CategoryId>,
        name: &str,
    ) -> Result<u64> {
        let result = sqlx::query(&self.pool.shared(
            "UPDATE categories SET parent_id = ?, name = ? WHERE id = ?",
        ))
            .bind(parent_id.map(|id| id as i64))
            .bind(name)
            .bind(id as i64)
            .execute(self.pool.any())
            .await?;
        Ok(result.rows_affected())
    }

    /// 该分类下的账单数量，用于删除前的友好提示。
    pub async fn count_bills(&self, id: CategoryId) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as(&self.pool.shared("SELECT COUNT(*) FROM bills WHERE category_id = ?"))
            .bind(id as i64)
            .fetch_one(self.pool.any())
            .await?;
        Ok(count)
    }

    /// 统计这些分类下的账单数量（递归删除前的预检）。
    ///
    /// `category_id = ANY(?)` 在 `sqlx::Any` 下绑不了数组（`Vec<i64>` 没有 Encode 实现），
    /// 改成按数量拼 `IN (?, ?, ...)` —— PostgreSQL 同认。
    pub async fn count_bills_in(&self, ids: &[CategoryId]) -> Result<i64> {
        if ids.is_empty() {
            return Ok(0);
        }

        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = self.pool.shared(&format!(
            "SELECT COUNT(*) FROM bills WHERE category_id IN ({placeholders})"
        ));

        let mut query = sqlx::query_as::<_, (i64,)>(&sql);
        for id in ids {
            query = query.bind(*id as i64);
        }

        let (count,) = query.fetch_one(self.pool.any()).await?;
        Ok(count)
    }

    /// 按给定顺序逐条删除，整体放在一个事务里。
    ///
    /// 调用方需要把子分类排在父分类前面：外键是 `ON DELETE RESTRICT`，
    /// 先删父会被拦住。
    pub async fn delete_many(&self, ids: &[CategoryId]) -> Result<u64> {
        let mut tx = self.pool.any().begin().await?;
        let mut affected = 0;

        for id in ids {
            let result = sqlx::query(&self.pool.shared("DELETE FROM categories WHERE id = ?"))
                .bind(*id as i64)
                .execute(&mut *tx)
                .await?;
            affected += result.rows_affected();
        }

        tx.commit().await?;
        Ok(affected)
    }

    pub async fn delete(&self, id: CategoryId) -> Result<u64> {
        let result = sqlx::query(&self.pool.shared("DELETE FROM categories WHERE id = ?"))
            .bind(id as i64)
            .execute(self.pool.any())
            .await?;
        Ok(result.rows_affected())
    }
}
