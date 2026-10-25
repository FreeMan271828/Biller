//! 整库数据迁移：把一个库里的全部业务数据复制到另一个库。
//!
//! 为什么是「源库读出 → 目标库写入」而不是 `INSERT ... SELECT`：
//! 两个库通常是**不同的 PostgreSQL 实例**，一条 SQL 里没法跨实例引用。
//!
//! 目标库会被**先清空再写入**，成功后内容与源库完全一致——连主键、外键关系和
//! 自增序列一起搬过去，这样迁移完切过去就能无感继续用。
//!
//! 目前只支持 PostgreSQL → PostgreSQL：`TRUNCATE ... RESTART IDENTITY`、`setval`、
//! `inet_server_addr()` 都是 PG 专有语法，跨 SQLite 的方言转换还没做（会明确报错）。

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use sqlx::FromRow;

use crate::common::db::{DbKind, Pool};

/// 各表搬了多少行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub books: u64,
    pub categories: u64,
    pub bills: u64,
    pub months: u64,
    pub settings: u64,
    /// 银行同步账户（之前漏搬，迁移后「默认账户」会丢）
    pub bank_accounts: u64,
    /// 已导入流水（之前漏搬，迁移后历史流水会被当成新流水重新导入）
    pub bank_imports: u64,
    /// 店家记忆
    pub memories: u64,
    /// 关键词规则
    pub keywords: u64,
}

impl MigrationReport {
    /// 总行数。
    pub fn total(&self) -> u64 {
        self.books
            + self.categories
            + self.bills
            + self.months
            + self.settings
            + self.bank_accounts
            + self.bank_imports
            + self.memories
            + self.keywords
    }
}

/// 把 `source` 的全部业务数据搬到 `target`。
///
/// 整个过程在目标库的**一个事务**里完成：中途失败不会留下半份数据。
pub async fn migrate_all(source: &Pool, target: &Pool) -> Result<MigrationReport> {
    if source.kind() != DbKind::Postgres || target.kind() != DbKind::Postgres {
        bail!(
            "数据迁移目前只支持 PostgreSQL → PostgreSQL。\n\
             TRUNCATE / setval / inet_server_addr() 都是 PG 专有语法，跨 SQLite 的方言转换还没做。"
        );
    }

    // 先确认不是同一个库：否则下面的 TRUNCATE 会先把源数据抹掉，再"迁移"一份空的
    let source_identity = database_identity(source).await?;
    let target_identity = database_identity(target).await?;
    if source_identity == target_identity {
        bail!("目标库与源库是同一个数据库（{source_identity}），迁移会先清空它，已取消");
    }

    let mut report = MigrationReport::default();
    let mut tx = target.any().begin().await.context("开启目标库事务失败")?;

    // 清空目标库；RESTART IDENTITY 把序列归零，随后由 setval 统一校正。
    // 必须把银行同步的 4 张表也列进来：它们有外键指向账单/账本/分类，
    // 只 TRUNCATE 业务表（哪怕带 CASCADE）会让「目标库 = 源库副本」不成立。
    sqlx::raw_sql(
        "TRUNCATE bills, categories, bill_books, month_balances, app_state,
                  bank_accounts, bank_imports, merchant_memory, keyword_rules
         RESTART IDENTITY CASCADE",
    )
    .execute(&mut *tx)
    .await
    .context("清空目标库失败")?;

    // 账本（账单要引用它，先搬）
    let books: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM bill_books ORDER BY id")
        .fetch_all(source.any())
        .await
        .context("读取源库账本失败")?;
    for (id, name) in &books {
        sqlx::query(&target.shared("INSERT INTO bill_books (id, name) VALUES (?, ?)"))
            .bind(id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        report.books += 1;
    }

    // 分类：父分类必须先落库，否则自引用外键会拦住子分类
    let rows: Vec<(i64, Option<i64>, String)> =
        sqlx::query_as("SELECT id, parent_id, name FROM categories")
            .fetch_all(source.any())
            .await
            .context("读取源库分类失败")?;
    for (id, parent_id, name) in order_categories(&rows) {
        sqlx::query(&target.shared("INSERT INTO categories (id, parent_id, name) VALUES (?, ?, ?)"))
            .bind(id)
            .bind(parent_id)
            .bind(&name)
            .execute(&mut *tx)
            .await?;
        report.categories += 1;
    }

    // 账单：时间戳读出时格式化成 ISO 文本（sqlx::Any 不认 chrono 类型），写回时再转回去。
    // 布尔同样按整数搬（`sqlx::Any` 对 bool 的支持在两端不一致）。
    let bills: Vec<BillRow> = sqlx::query_as(
        "SELECT id,
                kind,
                amount,
                book_id,
                category_id,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at,
                remark,
                CASE WHEN excluded THEN 1 ELSE 0 END AS excluded
           FROM bills ORDER BY id",
    )
    .fetch_all(source.any())
    .await
    .context("读取源库账单失败")?;
    for bill in &bills {
        sqlx::query(&target.shared(
            "INSERT INTO bills (id, kind, amount, book_id, category_id, created_at, remark, excluded)
             VALUES (?, ?, ?, ?, ?, ?::timestamptz, ?, (? = 1))",
        ))
        .bind(bill.id)
        .bind(&bill.kind)
        .bind(bill.amount)
        .bind(bill.book_id)
        .bind(bill.category_id)
        .bind(&bill.created_at)
        .bind(&bill.remark)
        .bind(bill.excluded)
        .execute(&mut *tx)
        .await?;
        report.bills += 1;
    }

    // 每月起始金额（月份同样以 ISO 文本往返）
    let months: Vec<(String, i64)> =
        sqlx::query_as("SELECT to_char(month, 'YYYY-MM-DD') AS month, start_balance FROM month_balances ORDER BY month")
            .fetch_all(source.any())
            .await
            .context("读取源库月度起始金额失败")?;
    for (month, balance) in &months {
        sqlx::query(&target.shared(
            "INSERT INTO month_balances (month, start_balance) VALUES (?::date, ?)",
        ))
            .bind(month)
            .bind(balance)
            .execute(&mut *tx)
            .await?;
        report.months += 1;
    }

    // 应用级状态（上次使用的日期/账本、开销计划等）
    let settings: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM app_state ORDER BY key")
            .fetch_all(source.any())
            .await
            .context("读取源库应用状态失败")?;
    for (key, value) in &settings {
        sqlx::query(&target.shared("INSERT INTO app_state (key, value) VALUES (?, ?)"))
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        report.settings += 1;
    }

    // ===== 银行同步的 4 张表 =====
    // 之前只搬业务表，这 4 张表被 TRUNCATE 之后没有补回来：
    // 迁移完银行同步账户丢失、已导入流水的指纹丢失，下次同步会把历史流水全部重新导入一遍。

    // 同步账户（is_default 按整数搬，写回时用 `(? = 1)` 兼容 PG boolean）
    let accounts: Vec<(i64, String, String, String, i64)> = sqlx::query_as(
        "SELECT id,
                account_uid,
                sub_account_no,
                alias,
                CASE WHEN is_default THEN 1 ELSE 0 END AS is_default
           FROM bank_accounts ORDER BY id",
    )
    .fetch_all(source.any())
    .await
    .context("读取源库同步账户失败")?;
    for (id, account_uid, sub_account_no, alias, is_default) in &accounts {
        sqlx::query(&target.shared(
            "INSERT INTO bank_accounts (id, account_uid, sub_account_no, alias, is_default)
             VALUES (?, ?, ?, ?, (? = 1))",
        ))
        .bind(id)
        .bind(account_uid)
        .bind(sub_account_no)
        .bind(alias)
        .bind(is_default)
        .execute(&mut *tx)
        .await?;
        report.bank_accounts += 1;
    }

    // 已导入流水；bill_id 指向刚搬完的账单
    let imports: Vec<(String, String, i64, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT fingerprint,
                to_char(txn_date, 'YYYY-MM-DD') AS txn_date,
                txn_money,
                memo,
                bill_id,
                to_char(imported_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS imported_at
           FROM bank_imports ORDER BY fingerprint",
    )
    .fetch_all(source.any())
    .await
    .context("读取源库已导入流水失败")?;
    for (fingerprint, txn_date, txn_money, memo, bill_id, imported_at) in &imports {
        sqlx::query(&target.shared(
            "INSERT INTO bank_imports (fingerprint, txn_date, txn_money, memo, bill_id, imported_at)
             VALUES (?, ?::date, ?, ?, ?, ?::timestamptz)",
        ))
        .bind(fingerprint)
        .bind(txn_date)
        .bind(txn_money)
        .bind(memo)
        .bind(bill_id)
        .bind(imported_at)
        .execute(&mut *tx)
        .await?;
        report.bank_imports += 1;
    }

    // 店家记忆
    let memories: Vec<(String, i64, i64, i64, String)> = sqlx::query_as(
        "SELECT memo_key, book_id, category_id, hits,
                to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_used_at
           FROM merchant_memory ORDER BY memo_key",
    )
    .fetch_all(source.any())
    .await
    .context("读取源库店家记忆失败")?;
    for (memo_key, book_id, category_id, hits, last_used_at) in &memories {
        sqlx::query(&target.shared(
            "INSERT INTO merchant_memory (memo_key, book_id, category_id, hits, last_used_at)
             VALUES (?, ?, ?, ?, ?::timestamptz)",
        ))
        .bind(memo_key)
        .bind(book_id)
        .bind(category_id)
        .bind(hits)
        .bind(last_used_at)
        .execute(&mut *tx)
        .await?;
        report.memories += 1;
    }

    // 关键词规则
    let keywords: Vec<(String, i64, i64, String)> = sqlx::query_as(
        "SELECT keyword, category_id, hits,
                to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_used_at
           FROM keyword_rules ORDER BY keyword",
    )
    .fetch_all(source.any())
    .await
    .context("读取源库关键词规则失败")?;
    for (keyword, category_id, hits, last_used_at) in &keywords {
        sqlx::query(&target.shared(
            "INSERT INTO keyword_rules (keyword, category_id, hits, last_used_at)
             VALUES (?, ?, ?, ?::timestamptz)",
        ))
        .bind(keyword)
        .bind(category_id)
        .bind(hits)
        .bind(last_used_at)
        .execute(&mut *tx)
        .await?;
        report.keywords += 1;
    }

    // 主键是显式搬过来的，自增序列必须跟着对齐，否则下一次插入就会撞主键
    for table in ["bill_books", "categories", "bills", "bank_accounts"] {
        sqlx::raw_sql(&format!(
            "SELECT setval(pg_get_serial_sequence('{table}', 'id'),
                           COALESCE(MAX(id), 1), MAX(id) IS NOT NULL)
               FROM {table}"
        ))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("校正 {table} 的自增序列失败"))?;
    }

    tx.commit().await.context("提交目标库事务失败")?;
    Ok(report)
}

/// 源库账单的原始行；时间戳是 ISO 文本，布尔是 0/1 整数。
#[derive(FromRow)]
struct BillRow {
    id: i64,
    kind: String,
    amount: i64,
    book_id: i64,
    category_id: i64,
    created_at: String,
    remark: Option<String>,
    excluded: i64,
}

/// 连接的唯一标识：`库名@服务器地址:端口`。
///
/// 用服务器自己报告的地址，而不是连接串文本——`localhost` 与 `127.0.0.1`
/// 写起来不同，但指向的其实是同一个库。
async fn database_identity(pool: &Pool) -> Result<String> {
    let (database, host, port): (String, Option<String>, Option<i32>) =
        sqlx::query_as("SELECT current_database(), inet_server_addr()::text, inet_server_port()")
            .fetch_one(pool.any())
            .await
            .context("读取数据库标识失败")?;

    Ok(format!(
        "{database}@{}:{}",
        host.unwrap_or_else(|| "?".to_string()),
        port.unwrap_or(0)
    ))
}

/// 让父分类排在子分类前面。
///
/// 外键已经保证源数据不会成环，这里的循环只是防御：万一遇到异常数据，
/// 剩下的原样追加——宁可让插入报错，也不悄悄丢数据。
fn order_categories(rows: &[(i64, Option<i64>, String)]) -> Vec<(i64, Option<i64>, String)> {
    let mut remaining = rows.to_vec();
    let mut placed: HashSet<i64> = HashSet::new();
    let mut ordered = Vec::with_capacity(rows.len());

    loop {
        let mut progressed = false;
        let mut index = 0;

        while index < remaining.len() {
            let (id, parent, _) = &remaining[index];
            let ready = match parent {
                None => true,
                Some(parent) => placed.contains(parent),
            };

            if ready {
                placed.insert(*id);
                ordered.push(remaining.remove(index));
                progressed = true;
            } else {
                index += 1;
            }
        }

        if !progressed {
            break;
        }
    }

    ordered.extend(remaining);
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_parents_before_children() {
        // 故意把子分类排在父分类前面
        let rows = vec![
            (3, Some(2), "简餐".to_string()),
            (1, None, "餐饮".to_string()),
            (2, Some(1), "午餐".to_string()),
        ];

        let ids: Vec<i64> = order_categories(&rows).iter().map(|(id, ..)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn orders_deep_chains() {
        let rows = vec![
            (4, Some(3), "第四层".to_string()),
            (3, Some(2), "第三层".to_string()),
            (2, Some(1), "第二层".to_string()),
            (1, None, "第一层".to_string()),
        ];

        let ids: Vec<i64> = order_categories(&rows).iter().map(|(id, ..)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn keeps_rows_with_missing_parent() {
        // 外键理论上不允许，但真出现了也不能丢数据
        let rows = vec![(1, Some(999), "孤儿".to_string())];
        assert_eq!(order_categories(&rows).len(), 1);
    }

    #[test]
    fn counts_rows() {
        let report = MigrationReport {
            books: 2,
            categories: 3,
            bills: 5,
            months: 1,
            settings: 2,
            bank_accounts: 1,
            bank_imports: 4,
            memories: 2,
            keywords: 3,
        };
        assert_eq!(report.total(), 23, "银行同步的 4 张表也要算进总数");
    }

    /// 跨后端迁移必须明确拒绝，不能把 SQLite 库当成 PG 跑一半。
    #[tokio::test]
    async fn refuses_to_migrate_against_sqlite() {
        use crate::common::db;

        let dir = std::env::temp_dir().join(format!(
            "biller-migrate-guard-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let pool = db::connect(&db::sqlite_url(&dir.join("biller.db")))
            .await
            .expect("连接临时 SQLite");
        db::init_schema(&pool).await.expect("建表");

        let error = migrate_all(&pool, &pool)
            .await
            .expect_err("SQLite 之间不允许迁移");
        assert!(
            error.to_string().contains("只支持 PostgreSQL"),
            "错误提示应说明只支持 PG → PG：{error}"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
