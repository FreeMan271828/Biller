//! 账单的数据访问层：只写 SQL，不做业务判断。

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::bill::model::{BillEntity, BillKind};
use crate::bill_book::model::BillBook;
use crate::category::model::Category;
use crate::common::types::{AmountType, BillId, BookId, CategoryId};

/// 账单查询条件；账本/分类名称已由 service 解析成 id。
#[derive(Debug, Clone, Default)]
pub struct BillFilter {
    pub month: Option<NaiveDate>,
    /// 按年筛选（与 `month` 互不冲突，可同时给）。
    pub year: Option<i32>,
    pub book_id: Option<BookId>,
    pub category_id: Option<CategoryId>,
    pub kind: Option<BillKind>,
    pub limit: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct BillRow {
    id: i64,
    kind: String,
    amount: i64,
    book_id: i64,
    book_name: String,
    category_id: i64,
    category_name: String,
    category_parent_id: Option<i64>,
    created_at: DateTime<Utc>,
    remark: Option<String>,
}

impl TryFrom<BillRow> for BillEntity {
    type Error = anyhow::Error;

    fn try_from(row: BillRow) -> Result<Self> {
        let kind = BillKind::parse(&row.kind)
            .ok_or_else(|| anyhow::anyhow!("数据库里出现未知的账单方向：{}", row.kind))?;
        Ok(Self {
            id: row.id as BillId,
            kind,
            amount: row.amount as AmountType,
            book: BillBook {
                id: row.book_id as BookId,
                name: row.book_name,
            },
            category: Category {
                id: row.category_id as CategoryId,
                parent_id: row.category_parent_id.map(|id| id as CategoryId),
                name: row.category_name,
            },
            created_at: row.created_at,
            remark: row.remark,
        })
    }
}

/// 月份归属按 UTC 计算，与 `BillEntity::created_at.date_naive()` 保持一致。
const SELECT_BILLS: &str = r#"
SELECT b.id,
       b.kind,
       b.amount,
       bk.id       AS book_id,
       bk.name     AS book_name,
       c.id        AS category_id,
       c.name      AS category_name,
       c.parent_id AS category_parent_id,
       b.created_at,
       b.remark
  FROM bills b
  JOIN bill_books bk ON bk.id = b.book_id
  JOIN categories c  ON c.id = b.category_id
 WHERE ($1::date IS NULL
        OR date_trunc('month', b.created_at AT TIME ZONE 'UTC')::date = $1::date)
   AND ($2::int IS NULL
        OR date_part('year', b.created_at AT TIME ZONE 'UTC')::int = $2)
   AND ($3::bigint IS NULL OR b.book_id = $3)
   AND ($4::bigint IS NULL OR b.category_id = $4)
   AND ($5::text   IS NULL OR b.kind = $5)
 ORDER BY b.created_at DESC, b.id DESC
 LIMIT COALESCE($6::bigint, 9223372036854775807)
"#;

/// 单条账单，列与 [`SELECT_BILLS`] 保持一致。
const SELECT_BILL_BY_ID: &str = r#"
SELECT b.id,
       b.kind,
       b.amount,
       bk.id       AS book_id,
       bk.name     AS book_name,
       c.id        AS category_id,
       c.name      AS category_name,
       c.parent_id AS category_parent_id,
       b.created_at,
       b.remark
  FROM bills b
  JOIN bill_books bk ON bk.id = b.book_id
  JOIN categories c  ON c.id = b.category_id
 WHERE b.id = $1
"#;

#[derive(Clone)]
pub struct BillDao {
    pool: PgPool,
}

impl BillDao {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn insert(
        &self,
        kind: BillKind,
        amount: AmountType,
        book_id: BookId,
        category_id: CategoryId,
        created_at: DateTime<Utc>,
        remark: Option<&str>,
    ) -> Result<BillId> {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO bills (kind, amount, book_id, category_id, created_at, remark)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id",
        )
        .bind(kind.as_str())
        .bind(amount)
        .bind(book_id as i64)
        .bind(category_id as i64)
        .bind(created_at)
        .bind(remark)
        .fetch_one(&self.pool)
        .await?;
        Ok(id as BillId)
    }

    pub async fn list(&self, filter: &BillFilter) -> Result<Vec<BillEntity>> {
        let rows = sqlx::query_as::<_, BillRow>(SELECT_BILLS)
            .bind(filter.month)
            .bind(filter.year)
            .bind(filter.book_id.map(|id| id as i64))
            .bind(filter.category_id.map(|id| id as i64))
            .bind(filter.kind.map(|kind| kind.as_str()))
            .bind(filter.limit)
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter().map(BillEntity::try_from).collect()
    }

    /// 按 id 取单条账单（供编辑预填）。
    pub async fn get(&self, id: BillId) -> Result<Option<BillEntity>> {
        let row = sqlx::query_as::<_, BillRow>(SELECT_BILL_BY_ID)
            .bind(id as i64)
            .fetch_optional(&self.pool)
            .await?;
        row.map(BillEntity::try_from).transpose()
    }

    /// 覆盖式更新一条账单；返回受影响行数。
    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        id: BillId,
        kind: BillKind,
        amount: AmountType,
        book_id: BookId,
        category_id: CategoryId,
        created_at: DateTime<Utc>,
        remark: Option<&str>,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE bills
                SET kind = $1, amount = $2, book_id = $3, category_id = $4,
                    created_at = $5, remark = $6
              WHERE id = $7",
        )
        .bind(kind.as_str())
        .bind(amount)
        .bind(book_id as i64)
        .bind(category_id as i64)
        .bind(created_at)
        .bind(remark)
        .bind(id as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete(&self, id: BillId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM bills WHERE id = $1")
            .bind(id as i64)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
