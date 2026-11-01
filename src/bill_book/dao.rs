//! 账本的数据访问层：只写 SQL，不做业务判断。

use crate::common::db::Pool;
use anyhow::Result;

use crate::bill_book::model::{BillBook, Plan, PlanKind};
use crate::common::types::BookId;

#[derive(sqlx::FromRow)]
struct BookRow {
    id: i64,
    name: String,
    plan_limit: Option<i64>,
    plan_kind: String,
    /// 0/1：SQLite 本来就是整数，PG 侧用 CASE 转成整数，sqlx::Any 才能统一解。
    in_calendar: i64,
}

impl From<BookRow> for BillBook {
    fn from(row: BookRow) -> Self {
        Self {
            id: row.id as BookId,
            name: row.name,
        }
    }
}

impl From<BookRow> for Plan {
    fn from(row: BookRow) -> Self {
        Self {
            book: BillBook {
                id: row.id as BookId,
                name: row.name,
            },
            limit: row.plan_limit,
            kind: PlanKind::from_code(&row.plan_kind),
            in_calendar: row.in_calendar != 0,
        }
    }
}

#[derive(Clone)]
pub struct BookDao {
    pool: Pool,
}

impl BookDao {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, name: &str, limit: Option<i64>, kind: PlanKind) -> Result<BookId> {
        let (id,): (i64,) = sqlx::query_as(
            &self.pool.shared(
                "INSERT INTO bill_books (name, plan_limit, plan_kind) VALUES (?, ?, ?) RETURNING id",
            ),
        )
        .bind(name)
        .bind(limit)
        .bind(kind.code())
        .fetch_one(self.pool.any())
        .await?;
        Ok(id as BookId)
    }

    pub async fn list(&self) -> Result<Vec<BillBook>> {
        let rows = sqlx::query_as::<_, BookRow>(
            &self
                .pool
                .shared("SELECT id, name, plan_limit, plan_kind, CASE WHEN in_calendar THEN 1 ELSE 0 END AS in_calendar FROM bill_books ORDER BY id"),
        )
        .fetch_all(self.pool.any())
        .await?;
        Ok(rows.into_iter().map(BillBook::from).collect())
    }

    /// 计划列表：账本 + 上限。计划页和概览的额度条都用它。
    pub async fn list_plans(&self) -> Result<Vec<Plan>> {
        let rows = sqlx::query_as::<_, BookRow>(
            &self
                .pool
                .shared("SELECT id, name, plan_limit, plan_kind, CASE WHEN in_calendar THEN 1 ELSE 0 END AS in_calendar FROM bill_books ORDER BY id"),
        )
        .fetch_all(self.pool.any())
        .await?;
        Ok(rows.into_iter().map(Plan::from).collect())
    }

    pub async fn get(&self, id: BookId) -> Result<Option<BillBook>> {
        let row = sqlx::query_as::<_, BookRow>(
            &self
                .pool
                .shared("SELECT id, name, plan_limit, plan_kind, CASE WHEN in_calendar THEN 1 ELSE 0 END AS in_calendar FROM bill_books WHERE id = ?"),
        )
        .bind(id as i64)
        .fetch_optional(self.pool.any())
        .await?;
        Ok(row.map(BillBook::from))
    }

    pub async fn find_by_name(&self, name: &str) -> Result<Option<BillBook>> {
        let row = sqlx::query_as::<_, BookRow>(
            &self
                .pool
                .shared("SELECT id, name, plan_limit, plan_kind, CASE WHEN in_calendar THEN 1 ELSE 0 END AS in_calendar FROM bill_books WHERE name = ?"),
        )
        .bind(name)
        .fetch_optional(self.pool.any())
        .await?;
        Ok(row.map(BillBook::from))
    }

    /// 设置「是否计入日历」；返回受影响行数。
    pub async fn set_in_calendar(&self, id: BookId, in_calendar: bool) -> Result<u64> {
        let result = sqlx::query(
            &self
                .pool
                .shared("UPDATE bill_books SET in_calendar = (? = 1) WHERE id = ?"),
        )
        .bind(if in_calendar { 1_i64 } else { 0_i64 })
        .bind(id as i64)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected())
    }


    /// 设置上限（`None` = 不设上限）；返回受影响行数。
    pub async fn set_limit(&self, id: BookId, limit: Option<i64>) -> Result<u64> {
        let result = sqlx::query(
            &self
                .pool
                .shared("UPDATE bill_books SET plan_limit = ? WHERE id = ?"),
        )
        .bind(limit)
        .bind(id as i64)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected())
    }

    /// 改类型（收入 / 开销）；返回受影响行数。
    pub async fn set_kind(&self, id: BookId, kind: PlanKind) -> Result<u64> {
        let result = sqlx::query(
            &self
                .pool
                .shared("UPDATE bill_books SET plan_kind = ? WHERE id = ?"),
        )
        .bind(kind.code())
        .bind(id as i64)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected())
    }

    /// 改名；返回受影响行数。
    pub async fn update(&self, id: BookId, name: &str) -> Result<u64> {
        let result = sqlx::query(
            &self
                .pool
                .shared("UPDATE bill_books SET name = ? WHERE id = ?"),
        )
        .bind(name)
        .bind(id as i64)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected())
    }

    /// 该账本下的账单数量，用于删除前的友好提示。
    pub async fn count_bills(&self, id: BookId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            &self
                .pool
                .shared("SELECT COUNT(*) FROM bills WHERE book_id = ?"),
        )
        .bind(id as i64)
        .fetch_one(self.pool.any())
        .await?;
        Ok(count)
    }

    pub async fn delete(&self, id: BookId) -> Result<u64> {
        let result = sqlx::query(&self.pool.shared("DELETE FROM bill_books WHERE id = ?"))
            .bind(id as i64)
            .execute(self.pool.any())
            .await?;
        Ok(result.rows_affected())
    }

    /// 把某个计划名下的账单整批挪到另一个计划；返回挪走的账单数。
    pub async fn move_bills(&self, from: BookId, to: BookId) -> Result<u64> {
        let result = sqlx::query(
            &self
                .pool
                .shared("UPDATE bills SET book_id = ? WHERE book_id = ?"),
        )
        .bind(to as i64)
        .bind(from as i64)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected())
    }
}
