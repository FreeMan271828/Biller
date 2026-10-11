//! 账单的数据访问层：只写 SQL，不做业务判断。
//!
//! 两个后端的差异都收敛在这里：
//!   * 占位符统一 `?`（`sqlx::Any` 的约定，PostgreSQL 侧由 sqlx 重写成 `$n`）；
//!   * 时间戳在 PG 里是 `timestamptz`、在 SQLite 里是 `TEXT`，
//!     但**读出来都格式化成 `YYYY-MM-DD HH:MM:SS` 文本**——`Any` 不认 chrono 类型，
//!     所以时间全程以字符串进出，转换放在这里；
//!   * 月份/年份筛选两边都绑 ISO 文本，SQLite 侧用 `substr` 取前缀比对，
//!     这样 bind 值完全一致，不必分叉绑定。

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};

use crate::bill::model::{BillEntity, BillKind};
use crate::bill_book::model::BillBook;
use crate::category::model::Category;
use crate::common::db::Pool;
use crate::common::time;
use crate::common::types::{AmountType, BillId, BookId, CategoryId};

/// 时间戳进出统一用这个格式：`chrono` 的 `%Y-%m-%d %H:%M:%S`，UTC。
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

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
    /// 数据库里的时间戳，读出来时已格式化成文本。
    created_at: String,
    remark: Option<String>,
    /// 布尔在 SQLite 里是 0/1 的 INTEGER，PG 里是 boolean：
    /// SQL 里统一转成整数，这里再还原，免得 `bool` 解码在 SQLite 上翻车。
    excluded: i64,
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
            created_at: time::parse_datetime(Some(&row.created_at))?,
            remark: row.remark,
            excluded: row.excluded != 0,
        })
    }
}

/// 列表查询（PostgreSQL）：月份归属按 UTC 计算，与 `date_naive()` 一致。
const SELECT_BILLS_PG: &str = r#"
SELECT b.id,
       b.kind,
       b.amount,
       bk.id       AS book_id,
       bk.name     AS book_name,
       c.id        AS category_id,
       c.name      AS category_name,
       c.parent_id AS category_parent_id,
       to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at,
       b.remark,
       CASE WHEN b.excluded THEN 1 ELSE 0 END AS excluded
  FROM bills b
  JOIN bill_books bk ON bk.id = b.book_id
  JOIN categories c  ON c.id = b.category_id
 WHERE (?::date IS NULL
        OR date_trunc('month', b.created_at AT TIME ZONE 'UTC')::date = ?::date)
   AND (?::int IS NULL
        OR date_part('year', b.created_at AT TIME ZONE 'UTC')::int = ?::int)
   AND (?::bigint IS NULL OR b.book_id = ?)
   AND (?::bigint IS NULL OR b.category_id = ?)
   AND (?::text   IS NULL OR b.kind = ?)
 ORDER BY b.created_at DESC, b.id DESC
 LIMIT COALESCE(?::bigint, 9223372036854775807)
"#;

/// 列表查询（SQLite）：时间戳是 ISO 文本，月份/年份用 `strftime` 取。
const SELECT_BILLS_SQLITE: &str = r#"
SELECT b.id,
       b.kind,
       b.amount,
       bk.id       AS book_id,
       bk.name     AS book_name,
       c.id        AS category_id,
       c.name      AS category_name,
       c.parent_id AS category_parent_id,
       strftime('%Y-%m-%d %H:%M:%S', b.created_at) AS created_at,
       b.remark,
       CAST(b.excluded AS INTEGER) AS excluded
  FROM bills b
  JOIN bill_books bk ON bk.id = b.book_id
  JOIN categories c  ON c.id = b.category_id
 WHERE (? IS NULL
        OR strftime('%Y-%m', b.created_at) = substr(?, 1, 7))
   AND (? IS NULL
        OR strftime('%Y', b.created_at) = ?)
   AND (? IS NULL OR b.book_id = ?)
   AND (? IS NULL OR b.category_id = ?)
   AND (? IS NULL OR b.kind = ?)
 ORDER BY b.created_at DESC, b.id DESC
 LIMIT COALESCE(?, -1)
"#;

const SELECT_BILL_BY_ID_PG: &str = r#"
SELECT b.id,
       b.kind,
       b.amount,
       bk.id       AS book_id,
       bk.name     AS book_name,
       c.id        AS category_id,
       c.name      AS category_name,
       c.parent_id AS category_parent_id,
       to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at,
       b.remark,
       CASE WHEN b.excluded THEN 1 ELSE 0 END AS excluded
  FROM bills b
  JOIN bill_books bk ON bk.id = b.book_id
  JOIN categories c  ON c.id = b.category_id
 WHERE b.id = ?
"#;

const SELECT_BILL_BY_ID_SQLITE: &str = r#"
SELECT b.id,
       b.kind,
       b.amount,
       bk.id       AS book_id,
       bk.name     AS book_name,
       c.id        AS category_id,
       c.name      AS category_name,
       c.parent_id AS category_parent_id,
       strftime('%Y-%m-%d %H:%M:%S', b.created_at) AS created_at,
       b.remark,
       CAST(b.excluded AS INTEGER) AS excluded
  FROM bills b
  JOIN bill_books bk ON bk.id = b.book_id
  JOIN categories c  ON c.id = b.category_id
 WHERE b.id = ?
"#;

/// 时间戳 → 文本（UTC）。
fn timestamp_text(value: DateTime<Utc>) -> String {
    value.format(TIMESTAMP_FORMAT).to_string()
}

#[derive(Clone)]
pub struct BillDao {
    pool: Pool,
}

impl BillDao {
    pub fn new(pool: Pool) -> Self {
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
        let sql = self.pool.sql(
            "INSERT INTO bills (kind, amount, book_id, category_id, created_at, remark)
             VALUES (?, ?, ?, ?, ?::timestamptz, ?)
             RETURNING id",
            "INSERT INTO bills (kind, amount, book_id, category_id, created_at, remark)
             VALUES (?, ?, ?, ?, ?, ?)
             RETURNING id",
        );

        let (id,): (i64,) = sqlx::query_as(&sql)
            .bind(kind.as_str())
            .bind(amount)
            .bind(book_id as i64)
            .bind(category_id as i64)
            .bind(timestamp_text(created_at))
            .bind(remark)
            .fetch_one(self.pool.any())
            .await?;

        Ok(id as BillId)
    }

    pub async fn list(&self, filter: &BillFilter) -> Result<Vec<BillEntity>> {
        let sql = self.pool.sql(SELECT_BILLS_PG, SELECT_BILLS_SQLITE);

        // 月份/年份都绑 ISO 文本。
        // 注意绑定个数要跟 SQL 里的 `?` 一一对应：每个条件都出现两次
        // （一次判空、一次比较），所以 book/category/kind 也要各绑两次。
        let month = filter.month.map(|value| value.to_string());
        let year = filter.year.map(|value| value.to_string());
        let book = filter.book_id.map(|id| id as i64);
        let category = filter.category_id.map(|id| id as i64);
        let kind = filter.kind.map(|value| value.as_str());

        let rows = sqlx::query_as::<_, BillRow>(&sql)
            .bind(&month)
            .bind(&month)
            .bind(&year)
            .bind(&year)
            .bind(book)
            .bind(book)
            .bind(category)
            .bind(category)
            .bind(kind)
            .bind(kind)
            .bind(filter.limit)
            .fetch_all(self.pool.any())
            .await?;

        rows.into_iter().map(BillEntity::try_from).collect()
    }

    /// 按 id 取单条账单（供编辑预填）。
    pub async fn get(&self, id: BillId) -> Result<Option<BillEntity>> {
        let sql = self.pool.sql(SELECT_BILL_BY_ID_PG, SELECT_BILL_BY_ID_SQLITE);

        let row = sqlx::query_as::<_, BillRow>(&sql)
            .bind(id as i64)
            .fetch_optional(self.pool.any())
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
        let sql = self.pool.sql(
            "UPDATE bills
                SET kind = ?, amount = ?, book_id = ?, category_id = ?,
                    created_at = ?::timestamptz, remark = ?
              WHERE id = ?",
            "UPDATE bills
                SET kind = ?, amount = ?, book_id = ?, category_id = ?,
                    created_at = ?, remark = ?
              WHERE id = ?",
        );

        let result = sqlx::query(&sql)
            .bind(kind.as_str())
            .bind(amount)
            .bind(book_id as i64)
            .bind(category_id as i64)
            .bind(timestamp_text(created_at))
            .bind(remark)
            .bind(id as i64)
            .execute(self.pool.any())
            .await?;

        Ok(result.rows_affected())
    }

    pub async fn delete(&self, id: BillId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM bills WHERE id = ?")
            .bind(id as i64)
            .execute(self.pool.any())
            .await?;
        Ok(result.rows_affected())
    }

    /// 只切换「是否计入日历」，不动账单本身。
    pub async fn set_excluded(&self, id: BillId, excluded: bool) -> Result<u64> {
        // `(? = 1)` 让 PostgreSQL 的 boolean 与 SQLite 的 0/1 INTEGER 都吃得下，
        // 不必为「绑 bool 还是绑整数」分叉
        let result = sqlx::query("UPDATE bills SET excluded = (? = 1) WHERE id = ?")
            .bind(if excluded { 1_i64 } else { 0_i64 })
            .bind(id as i64)
            .execute(self.pool.any())
            .await?;
        Ok(result.rows_affected())
    }
}
