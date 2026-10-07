//! 账本的数据访问层：只写 SQL，不做业务判断。

use anyhow::Result;
use sqlx::PgPool;

use crate::bill_book::model::BillBook;
use crate::common::types::BookId;

#[derive(sqlx::FromRow)]
struct BookRow {
    id: i64,
    name: String,
}

impl From<BookRow> for BillBook {
    fn from(row: BookRow) -> Self {
        Self {
            id: row.id as BookId,
            name: row.name,
        }
    }
}

#[derive(Clone)]
pub struct BookDao {
    pool: PgPool,
}

impl BookDao {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, name: &str) -> Result<BookId> {
        let (id,): (i64,) = sqlx::query_as("INSERT INTO bill_books (name) VALUES ($1) RETURNING id")
            .bind(name)
            .fetch_one(&self.pool)
            .await?;
        Ok(id as BookId)
    }

    pub async fn list(&self) -> Result<Vec<BillBook>> {
        let rows = sqlx::query_as::<_, BookRow>("SELECT id, name FROM bill_books ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(BillBook::from).collect())
    }

    pub async fn get(&self, id: BookId) -> Result<Option<BillBook>> {
        let row = sqlx::query_as::<_, BookRow>("SELECT id, name FROM bill_books WHERE id = $1")
            .bind(id as i64)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(BillBook::from))
    }

    pub async fn find_by_name(&self, name: &str) -> Result<Option<BillBook>> {
        let row = sqlx::query_as::<_, BookRow>("SELECT id, name FROM bill_books WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(BillBook::from))
    }

    /// 改名；返回受影响行数。
    pub async fn update(&self, id: BookId, name: &str) -> Result<u64> {
        let result = sqlx::query("UPDATE bill_books SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(id as i64)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// 该账本下的账单数量，用于删除前的友好提示。
    pub async fn count_bills(&self, id: BookId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bills WHERE book_id = $1")
            .bind(id as i64)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn delete(&self, id: BookId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM bill_books WHERE id = $1")
            .bind(id as i64)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
