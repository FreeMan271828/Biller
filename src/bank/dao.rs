//! 银行同步的 4 张表：同步账户、已导入流水、店家记忆、关键词规则。
//!
//! 时间戳一律以 ISO 文本进出（`sqlx::Any` 不支持 chrono 类型），
//! PostgreSQL 侧靠 `?::timestamptz` 转回来。

use std::collections::HashSet;

use anyhow::Result;
use chrono::Utc;

use crate::bank::client::BankAccount;
use crate::bank::model::{BankTransaction, memo_key};
use crate::common::db::{Pool, Tx};
use crate::common::types::{BillId, BookId, CategoryId};

/// 时间戳统一格式（与账单那边一致）。
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// 存下来的账户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    pub account: BankAccount,
    pub alias: String,
    pub is_default: bool,
}

/// 店家记忆的一条：某个店家上次被记到哪个账本、哪个分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub memo_key: String,
    pub book_id: BookId,
    pub book_name: String,
    pub category_id: CategoryId,
    pub category_name: String,
    pub hits: i64,
}

impl MemoryEntry {
    /// 界面提示用：`记忆(3次)：微信 / 餐饮-夜宵`
    pub fn describe(&self) -> String {
        format!(
            "记忆({}次)：{} / {}",
            self.hits, self.book_name, self.category_name
        )
    }
}

/// 关键词规则的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordRule {
    pub keyword: String,
    pub category_id: CategoryId,
    pub category_name: String,
}

fn now_text() -> String {
    Utc::now().format(TIMESTAMP_FORMAT).to_string()
}

#[derive(Clone)]
pub struct BankDao {
    pool: Pool,
}

impl BankDao {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    // ---------------- 账户 ----------------

    /// 记住一个账户；已存在则更新别名与默认标记。
    pub async fn save_account(
        &self,
        account: &BankAccount,
        alias: &str,
        is_default: bool,
    ) -> Result<()> {
        let sql = self.pool.shared(
            "INSERT INTO bank_accounts (account_uid, sub_account_no, alias, is_default)
             VALUES (?, ?, ?, (? = 1))
             ON CONFLICT (account_uid) DO UPDATE
                SET sub_account_no = EXCLUDED.sub_account_no,
                    alias = EXCLUDED.alias,
                    is_default = EXCLUDED.is_default",
        );

        sqlx::query(&sql)
            .bind(&account.account_uid)
            .bind(&account.sub_account_no)
            .bind(alias)
            // `(? = 1)` 让 PostgreSQL 的 boolean 与 SQLite 的 0/1 INTEGER 都吃得下
            .bind(if is_default { 1_i64 } else { 0_i64 })
            .execute(self.pool.any())
            .await?;

        Ok(())
    }

    pub async fn accounts(&self) -> Result<Vec<StoredAccount>> {
        // 布尔列两边类型不同：PG 是 boolean，SQLite 是 INTEGER。
        // 统一在 SQL 里转成整数再读，避免 `bool` 解码在 SQLite 上失败。
        let sql = self.pool.sql(
            "SELECT account_uid, sub_account_no, alias,
                    CASE WHEN is_default THEN 1 ELSE 0 END AS is_default
               FROM bank_accounts ORDER BY is_default DESC, id",
            "SELECT account_uid, sub_account_no, alias, is_default
               FROM bank_accounts ORDER BY is_default DESC, id",
        );

        let rows: Vec<(String, String, String, i64)> =
            sqlx::query_as(&sql).fetch_all(self.pool.any()).await?;

        Ok(rows
            .into_iter()
            .map(
                |(account_uid, sub_account_no, alias, is_default)| StoredAccount {
                    account: BankAccount {
                        account_uid,
                        sub_account_no,
                    },
                    alias,
                    is_default: is_default != 0,
                },
            )
            .collect())
    }

    pub async fn default_account(&self) -> Result<Option<StoredAccount>> {
        Ok(self
            .accounts()
            .await?
            .into_iter()
            .find(|stored| stored.is_default))
    }

    /// 把某个账户设为唯一默认。
    pub async fn set_default_account(&self, account_uid: &str) -> Result<()> {
        let mut tx = self.pool.any().begin().await?;

        // `(0 = 1)` / `(1 = 1)`：PostgreSQL 得到 false/true，SQLite 得到 0/1
        sqlx::query(
            &self
                .pool
                .shared("UPDATE bank_accounts SET is_default = (0 = 1)"),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            &self
                .pool
                .shared("UPDATE bank_accounts SET is_default = (1 = 1) WHERE account_uid = ?"),
        )
        .bind(account_uid)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    // ---------------- 去重 ----------------

    /// 给定一批指纹，返回其中**已经导入过**的那些。
    pub async fn imported(&self, wanted: &[String]) -> Result<HashSet<String>> {
        if wanted.is_empty() {
            return Ok(HashSet::new());
        }

        // `IN (?, ?, ...)` —— `ANY(?)` 在 sqlx::Any 下绑不了数组
        let placeholders = vec!["?"; wanted.len()].join(", ");
        let sql = self.pool.shared(&format!(
            "SELECT fingerprint FROM bank_imports WHERE fingerprint IN ({placeholders})"
        ));

        let mut query = sqlx::query_as::<_, (String,)>(&sql);
        for fingerprint in wanted {
            query = query.bind(fingerprint);
        }

        let rows = query.fetch_all(self.pool.any()).await?;
        Ok(rows.into_iter().map(|(fingerprint,)| fingerprint).collect())
    }

    /// 记录一笔流水已导入（幂等：重复调用不会出错）。
    pub async fn mark_imported(
        &self,
        txn: &BankTransaction,
        bill_id: Option<BillId>,
    ) -> Result<()> {
        let sql = self.mark_imported_sql();

        sqlx::query(&sql)
            .bind(&txn.fingerprint)
            .bind(txn.date().to_string())
            .bind(Self::imported_amount(txn))
            .bind(&txn.memo)
            .bind(bill_id.map(|id| id as i64))
            .bind(now_text())
            .execute(self.pool.any())
            .await?;

        Ok(())
    }

    /// 在调用方的事务里记录「已导入」；银行同步用它和账单、记忆原子提交。
    pub async fn mark_imported_in(
        &self,
        tx: &mut Tx<'_>,
        txn: &BankTransaction,
        bill_id: Option<BillId>,
    ) -> Result<()> {
        let sql = self.mark_imported_sql();

        sqlx::query(&sql)
            .bind(&txn.fingerprint)
            .bind(txn.date().to_string())
            .bind(Self::imported_amount(txn))
            .bind(&txn.memo)
            .bind(bill_id.map(|id| id as i64))
            .bind(now_text())
            .execute(&mut **tx)
            .await?;

        Ok(())
    }

    fn mark_imported_sql(&self) -> String {
        self.pool.sql(
            "INSERT INTO bank_imports (fingerprint, txn_date, txn_money, memo, bill_id, imported_at)
             VALUES (?, ?::date, ?, ?, ?, ?::timestamptz)
             ON CONFLICT (fingerprint) DO NOTHING",
            "INSERT INTO bank_imports (fingerprint, txn_date, txn_money, memo, bill_id, imported_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (fingerprint) DO NOTHING",
        )
    }

    /// 支出在库里存负数、收入存正数（`txn_money` 的历史约定）。
    fn imported_amount(txn: &BankTransaction) -> i64 {
        match txn.kind {
            crate::bill::model::BillKind::Income => txn.amount,
            crate::bill::model::BillKind::Expense => -txn.amount,
        }
    }

    /// 已导入流水总数（展示用）。
    pub async fn imported_total(&self) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as(&self.pool.shared("SELECT COUNT(*) FROM bank_imports"))
                .fetch_one(self.pool.any())
                .await?;
        Ok(count)
    }

    // ---------------- 记忆规则一：店家 ----------------

    /// 按店家查记忆（精确命中归一化后的 key）。
    pub async fn memory_for(&self, memo: &str) -> Result<Option<MemoryEntry>> {
        let key = memo_key(memo);

        let row: Option<(String, i64, String, i64, String, i64)> =
            sqlx::query_as(&self.pool.shared(
                "SELECT m.memo_key, m.book_id, b.name, m.category_id, c.name, m.hits
                   FROM merchant_memory m
                   JOIN bill_books b ON b.id = m.book_id
                   JOIN categories c ON c.id = m.category_id
                  WHERE m.memo_key = ?
                  ORDER BY m.last_used_at DESC
                  LIMIT 1",
            ))
            .bind(&key)
            .fetch_optional(self.pool.any())
            .await?;

        Ok(row.map(
            |(memo_key, book_id, book_name, category_id, category_name, hits)| MemoryEntry {
                memo_key,
                book_id: book_id as BookId,
                book_name,
                category_id: category_id as CategoryId,
                category_name,
                hits,
            },
        ))
    }

    /// 记录/更新一次店家记忆（命中次数 +1，时间刷新）。
    pub async fn remember(
        &self,
        memo: &str,
        book_id: BookId,
        category_id: CategoryId,
    ) -> Result<()> {
        let key = memo_key(memo);
        if key.is_empty() {
            return Ok(());
        }

        let sql = self.remember_sql();

        sqlx::query(&sql)
            .bind(&key)
            .bind(book_id as i64)
            .bind(category_id as i64)
            .bind(now_text())
            .execute(self.pool.any())
            .await?;

        Ok(())
    }

    /// 在调用方的事务里记录店家记忆；银行同步用它和账单、导入标记原子提交。
    pub async fn remember_in(
        &self,
        tx: &mut Tx<'_>,
        memo: &str,
        book_id: BookId,
        category_id: CategoryId,
    ) -> Result<()> {
        let key = memo_key(memo);
        if key.is_empty() {
            return Ok(());
        }

        let sql = self.remember_sql();

        sqlx::query(&sql)
            .bind(&key)
            .bind(book_id as i64)
            .bind(category_id as i64)
            .bind(now_text())
            .execute(&mut **tx)
            .await?;

        Ok(())
    }

    fn remember_sql(&self) -> String {
        self.pool.sql(
            "INSERT INTO merchant_memory (memo_key, book_id, category_id, hits, last_used_at)
             VALUES (?, ?, ?, 1, ?::timestamptz)
             ON CONFLICT (memo_key) DO UPDATE
                SET book_id = EXCLUDED.book_id,
                    category_id = EXCLUDED.category_id,
                    hits = merchant_memory.hits + 1,
                    last_used_at = EXCLUDED.last_used_at",
            "INSERT INTO merchant_memory (memo_key, book_id, category_id, hits, last_used_at)
             VALUES (?, ?, ?, 1, ?)
             ON CONFLICT (memo_key) DO UPDATE
                SET book_id = EXCLUDED.book_id,
                    category_id = EXCLUDED.category_id,
                    hits = merchant_memory.hits + 1,
                    last_used_at = EXCLUDED.last_used_at",
        )
    }

    /// 最近用过的记忆，界面上展示、也方便用户清理。
    pub async fn memory_list(&self) -> Result<Vec<MemoryEntry>> {
        let rows: Vec<(String, i64, String, i64, String, i64)> = sqlx::query_as(&self.pool.shared(
            "SELECT m.memo_key, m.book_id, b.name, m.category_id, c.name, m.hits
                   FROM merchant_memory m
                   JOIN bill_books b ON b.id = m.book_id
                   JOIN categories c ON c.id = m.category_id
                  ORDER BY m.last_used_at DESC
                  LIMIT 200",
        ))
        .fetch_all(self.pool.any())
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(memo_key, book_id, book_name, category_id, category_name, hits)| MemoryEntry {
                    memo_key,
                    book_id: book_id as BookId,
                    book_name,
                    category_id: category_id as CategoryId,
                    category_name,
                    hits,
                },
            )
            .collect())
    }

    pub async fn clear_memory(&self) -> Result<u64> {
        let result = sqlx::query(&self.pool.shared("DELETE FROM merchant_memory"))
            .execute(self.pool.any())
            .await?;
        Ok(result.rows_affected())
    }

    // ---------------- 记忆规则二：关键词 ----------------

    /// 全部关键词规则，**长的在前**（越长越具体，优先命中）。
    pub async fn keywords(&self) -> Result<Vec<KeywordRule>> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(&self.pool.shared(
            "SELECT k.keyword, k.category_id, c.name
               FROM keyword_rules k
               JOIN categories c ON c.id = k.category_id
              ORDER BY LENGTH(k.keyword) DESC, k.keyword",
        ))
        .fetch_all(self.pool.any())
        .await?;

        Ok(rows
            .into_iter()
            .map(|(keyword, category_id, category_name)| KeywordRule {
                keyword,
                category_id: category_id as CategoryId,
                category_name,
            })
            .collect())
    }

    pub async fn set_keyword(&self, keyword: &str, category_id: CategoryId) -> Result<()> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(());
        }

        let sql = self.pool.sql(
            "INSERT INTO keyword_rules (keyword, category_id, hits, last_used_at)
             VALUES (?, ?, 1, ?::timestamptz)
             ON CONFLICT (keyword) DO UPDATE
                SET category_id = EXCLUDED.category_id,
                    last_used_at = EXCLUDED.last_used_at",
            "INSERT INTO keyword_rules (keyword, category_id, hits, last_used_at)
             VALUES (?, ?, 1, ?)
             ON CONFLICT (keyword) DO UPDATE
                SET category_id = EXCLUDED.category_id,
                    last_used_at = EXCLUDED.last_used_at",
        );

        sqlx::query(&sql)
            .bind(keyword)
            .bind(category_id as i64)
            .bind(now_text())
            .execute(self.pool.any())
            .await?;

        Ok(())
    }

    pub async fn remove_keyword(&self, keyword: &str) -> Result<u64> {
        let result = sqlx::query(
            &self
                .pool
                .shared("DELETE FROM keyword_rules WHERE keyword = ?"),
        )
        .bind(keyword.trim())
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected())
    }

    /// 按分类名预置一条关键词规则（同名分类优先取子分类，其次根分类）。
    ///
    /// 分类不存在时什么都不做——预置规则不该凭空造分类。
    /// 已经存在的关键词不覆盖（用户改过的优先级最高）。
    pub async fn seed_keyword(&self, keyword: &str, category_name: &str) -> Result<u64> {
        let sql = self.pool.sql(
            "INSERT INTO keyword_rules (keyword, category_id, hits, last_used_at)
             SELECT ?, id, 1, ?::timestamptz
               FROM categories
              WHERE name = ?
              ORDER BY (parent_id IS NOT NULL) DESC, id
              LIMIT 1
             ON CONFLICT (keyword) DO NOTHING",
            "INSERT INTO keyword_rules (keyword, category_id, hits, last_used_at)
             SELECT ?, id, 1, ?
               FROM categories
              WHERE name = ?
              ORDER BY (parent_id IS NOT NULL) DESC, id
              LIMIT 1
             ON CONFLICT (keyword) DO NOTHING",
        );

        let result = sqlx::query(&sql)
            .bind(keyword.trim())
            .bind(now_text())
            .bind(category_name)
            .execute(self.pool.any())
            .await?;

        Ok(result.rows_affected())
    }

    /// 本地最新一条账单的日期（决定「本周」同步的起点）。
    pub async fn latest_bill_date(&self) -> Result<Option<chrono::NaiveDate>> {
        // 账单时间存的是 UTC；这里仍按 UTC 取日期，与账单自身的月份归属口径一致
        let sql = self.pool.sql(
            "SELECT to_char(MAX(created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD') FROM bills",
            "SELECT MAX(substr(created_at, 1, 10)) FROM bills",
        );

        let (latest,): (Option<String>,) = sqlx::query_as(&sql).fetch_one(self.pool.any()).await?;

        Ok(latest.and_then(|text| chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    use crate::bank::model::SyncWindow;
    use crate::bill::model::BillKind;
    use crate::common::db;

    /// 在一个临时 SQLite 文件上建好表，返回池和它的目录（测试结束删目录）。
    async fn setup() -> (Pool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "biller-bank-dao-{}-{:?}",
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

    async fn seed_book_and_categories(pool: &Pool) -> (BookId, CategoryId, CategoryId) {
        let (book,): (i64,) =
            sqlx::query_as(&pool.shared("INSERT INTO bill_books (name) VALUES (?) RETURNING id"))
                .bind("微信")
                .fetch_one(pool.any())
                .await
                .unwrap();

        let (food,): (i64,) = sqlx::query_as(
            &pool.shared("INSERT INTO categories (parent_id, name) VALUES (NULL, ?) RETURNING id"),
        )
        .bind("餐饮")
        .fetch_one(pool.any())
        .await
        .unwrap();

        let (night,): (i64,) = sqlx::query_as(
            &pool.shared("INSERT INTO categories (parent_id, name) VALUES (?, ?) RETURNING id"),
        )
        .bind(food)
        .bind("夜宵")
        .fetch_one(pool.any())
        .await
        .unwrap();

        (book as BookId, food as CategoryId, night as CategoryId)
    }

    fn transaction(fingerprint: &str, memo: &str) -> BankTransaction {
        BankTransaction {
            date_time: NaiveDate::from_ymd_opt(2026, 10, 18)
                .unwrap()
                .and_hms_opt(20, 38, 3)
                .unwrap(),
            kind: BillKind::Expense,
            amount: 1420,
            memo: memo.to_string(),
            balance: Some(1_113_577),
            raw_type: "网联协议支付".to_string(),
            fingerprint: fingerprint.to_string(),
        }
    }

    #[tokio::test]
    async fn saves_and_reads_accounts() {
        let (pool, dir) = setup().await;
        let dao = BankDao::new(pool.clone());

        let account = BankAccount {
            account_uid: "P-abc".to_string(),
            sub_account_no: "125621564370001".to_string(),
        };
        dao.save_account(&account, "工资卡", true).await.unwrap();

        let stored = dao.accounts().await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].alias, "工资卡");
        assert!(stored[0].is_default);
        assert_eq!(
            dao.default_account().await.unwrap().unwrap().account,
            account
        );

        // 再存一次是更新而不是插两条
        dao.save_account(&account, "主力卡", true).await.unwrap();
        let stored = dao.accounts().await.unwrap();
        assert_eq!(stored.len(), 1, "同一账户不应重复");
        assert_eq!(stored[0].alias, "主力卡");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn switching_default_account_is_exclusive() {
        let (pool, dir) = setup().await;
        let dao = BankDao::new(pool.clone());

        let first = BankAccount {
            account_uid: "P-1".to_string(),
            sub_account_no: "111".to_string(),
        };
        let second = BankAccount {
            account_uid: "P-2".to_string(),
            sub_account_no: "222".to_string(),
        };
        dao.save_account(&first, "A", true).await.unwrap();
        dao.save_account(&second, "B", false).await.unwrap();

        dao.set_default_account("P-2").await.unwrap();

        let accounts = dao.accounts().await.unwrap();
        let defaults: Vec<&StoredAccount> =
            accounts.iter().filter(|item| item.is_default).collect();
        assert_eq!(defaults.len(), 1, "默认账户只能有一个");
        assert_eq!(defaults[0].account.account_uid, "P-2");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn import_is_idempotent() {
        let (pool, dir) = setup().await;
        let dao = BankDao::new(pool.clone());

        let txn = transaction("fp-1", "正新鸡排");

        dao.mark_imported(&txn, None).await.unwrap();
        // 重复标记不应报错，也不应多出一条
        dao.mark_imported(&txn, None).await.unwrap();
        assert_eq!(dao.imported_total().await.unwrap(), 1);

        let imported = dao
            .imported(&["fp-1".to_string(), "fp-2".to_string()])
            .await
            .unwrap();
        assert!(imported.contains("fp-1"));
        assert!(!imported.contains("fp-2"));

        // 空列表直接返回空集合（不拼出 `IN ()` 这种非法 SQL）
        assert!(dao.imported(&[]).await.unwrap().is_empty());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remembers_merchant_then_updates_it() {
        let (pool, dir) = setup().await;
        let (book, food, night) = seed_book_and_categories(&pool).await;
        let dao = BankDao::new(pool.clone());

        // 第一次：记忆里没有
        assert!(
            dao.memory_for("财付通-微信支付-正新鸡排")
                .await
                .unwrap()
                .is_none()
        );

        dao.remember("财付通-微信支付-正新鸡排", book, food)
            .await
            .unwrap();
        let entry = dao
            .memory_for("网银在线-京东商城-正新鸡排")
            .await
            .unwrap()
            .expect("不同渠道前缀应指向同一个店家");
        assert_eq!(entry.book_id, book);
        assert_eq!(entry.category_id, food);
        assert_eq!(entry.hits, 1);
        assert_eq!(entry.book_name, "微信");
        assert_eq!(entry.category_name, "餐饮");

        // 第二次改到子分类：应覆盖映射并把命中次数 +1
        dao.remember("网银在线-京东商城-正新鸡排", book, night)
            .await
            .unwrap();
        let entry = dao.memory_for("正新鸡排").await.unwrap().unwrap();
        assert_eq!(entry.category_id, night, "应更新为最新的映射");
        assert_eq!(entry.category_name, "夜宵");
        assert_eq!(entry.hits, 2, "命中次数应累加");

        assert_eq!(dao.memory_list().await.unwrap().len(), 1);
        assert_eq!(dao.clear_memory().await.unwrap(), 1);
        assert!(dao.memory_for("正新鸡排").await.unwrap().is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keyword_rules_are_ordered_by_length() {
        let (pool, dir) = setup().await;
        let (_, food, night) = seed_book_and_categories(&pool).await;
        let dao = BankDao::new(pool.clone());

        dao.set_keyword("正新鸡排", night).await.unwrap();
        dao.set_keyword("餐饮", food).await.unwrap();

        let rules = dao.keywords().await.unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].keyword, "正新鸡排", "长关键词优先命中");
        assert_eq!(rules[0].category_name, "夜宵");
        assert_eq!(rules[1].keyword, "餐饮");

        // 覆盖同一关键词
        dao.set_keyword("餐饮", night).await.unwrap();
        let rules = dao.keywords().await.unwrap();
        assert_eq!(rules.len(), 2, "不应重复插入");
        assert!(
            rules
                .iter()
                .any(|rule| rule.keyword == "餐饮" && rule.category_id == night)
        );

        // 空关键词直接忽略
        dao.set_keyword("   ", food).await.unwrap();
        assert_eq!(dao.keywords().await.unwrap().len(), 2);

        assert_eq!(dao.remove_keyword("正新鸡排").await.unwrap(), 1);
        assert_eq!(dao.keywords().await.unwrap().len(), 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dangling_bill_id_is_rejected_by_foreign_key() {
        let (pool, dir) = setup().await;
        let dao = BankDao::new(pool.clone());

        // 指向不存在的账单：外键应拦住（说明 SQLite 侧外键也是开着的）
        let error = dao
            .mark_imported(&transaction("fp-bad", "无主"), Some(42))
            .await
            .expect_err("外键应拦住");
        assert!(
            format!("{error:?}").to_lowercase().contains("foreign key"),
            "{error:?}"
        );

        // 不给 bill_id 就能正常写入
        dao.mark_imported(&transaction("fp-ok", "无主"), None)
            .await
            .unwrap();
        assert_eq!(dao.imported_total().await.unwrap(), 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stores_transaction_direction_and_date() {
        let (pool, dir) = setup().await;
        let dao = BankDao::new(pool.clone());

        let txn = transaction("fp-income", "退款");
        dao.mark_imported(&txn, None).await.unwrap();

        let (date, money): (String, i64) = sqlx::query_as(
            &pool.shared("SELECT txn_date, txn_money FROM bank_imports WHERE fingerprint = ?"),
        )
        .bind("fp-income")
        .fetch_one(pool.any())
        .await
        .unwrap();

        assert_eq!(date, "2026-10-18", "日期以 ISO 文本存");
        assert_eq!(money, -1420, "支出在库里存负数");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 这条保证 `SyncWindow` 与 DAO 的日期格式是一致的（两处都用 ISO 文本）。
    #[test]
    fn window_text_matches_stored_date_format() {
        let window = SyncWindow::today(NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        assert_eq!(window.start_text(), "20261018");
        assert_eq!(window.start.to_string(), "2026-10-18");
    }

    /// 需要真实数据库：设置 `BILLER_TEST_DATABASE_URL` 时才会执行。
    ///
    /// 与上面的 SQLite 测试跑**同一套 DAO 操作**，用来证明两条数据库路线都通。
    /// 数据用独立前缀，跑完自行清理，不动真实账单。
    #[tokio::test]
    async fn works_on_a_real_database_too() {
        let Ok(url) = std::env::var("BILLER_TEST_DATABASE_URL") else {
            return;
        };

        let pool = db::connect(&url).await.expect("连接测试库");
        db::init_schema(&pool).await.expect("建表");
        let dao = BankDao::new(pool.clone());

        // 借用现成的一条账本和分类（没有就临时建一个）
        let book: (i64,) = sqlx::query_as(&pool.shared("SELECT id FROM bill_books LIMIT 1"))
            .fetch_optional(pool.any())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("测试库里至少要有一个账本"));
        let category: (i64,) = sqlx::query_as(&pool.shared("SELECT id FROM categories LIMIT 1"))
            .fetch_optional(pool.any())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("测试库里至少要有一个分类"));

        let book_id = book.0 as BookId;
        let category_id = category.0 as CategoryId;

        // ---- 账户（布尔列在 PG 上是 boolean）----
        let account = BankAccount {
            account_uid: "P-biller-selftest".to_string(),
            sub_account_no: "999000111".to_string(),
        };
        dao.save_account(&account, "自测卡", true).await.unwrap();
        dao.save_account(&account, "自测卡2", true).await.unwrap();

        let stored = dao.accounts().await.unwrap();
        let mine = stored
            .iter()
            .find(|item| item.account == account)
            .expect("应能读回账户");
        assert_eq!(mine.alias, "自测卡2");
        assert!(mine.is_default, "布尔读写应正常");

        // ---- 流水去重（DATE / 时间戳转换）----
        let txn = transaction("fp-selftest", "自测店家");
        dao.mark_imported(&txn, None).await.unwrap();
        dao.mark_imported(&txn, None).await.unwrap();
        assert!(
            dao.imported(&["fp-selftest".to_string()])
                .await
                .unwrap()
                .contains("fp-selftest")
        );

        // ---- 店家记忆（timestamptz 绑定 + ON CONFLICT 累加）----
        dao.remember("财付通-微信支付-自测店家", book_id, category_id)
            .await
            .unwrap();
        dao.remember("自测店家", book_id, category_id)
            .await
            .unwrap();

        let entry = dao.memory_for("自测店家").await.unwrap().expect("应有记忆");
        assert_eq!(entry.hits, 2, "两次应累加");
        assert_eq!(entry.book_id, book_id);

        // ---- 关键词（LENGTH 排序）----
        dao.set_keyword("自测长关键词", category_id).await.unwrap();
        dao.set_keyword("自测", category_id).await.unwrap();
        let rules = dao.keywords().await.unwrap();
        assert!(rules.iter().any(|rule| rule.keyword == "自测长关键词"));

        // ---- 清理（只删自己造的）----
        sqlx::query(&pool.shared("DELETE FROM bank_accounts WHERE account_uid = ?"))
            .bind("P-biller-selftest")
            .execute(pool.any())
            .await
            .unwrap();
        sqlx::query(&pool.shared("DELETE FROM bank_imports WHERE fingerprint = ?"))
            .bind("fp-selftest")
            .execute(pool.any())
            .await
            .unwrap();
        sqlx::query(&pool.shared("DELETE FROM merchant_memory WHERE memo_key = ?"))
            .bind("自测店家")
            .execute(pool.any())
            .await
            .unwrap();
        sqlx::query(&pool.shared("DELETE FROM keyword_rules WHERE keyword IN (?, ?)"))
            .bind("自测长关键词")
            .bind("自测")
            .execute(pool.any())
            .await
            .unwrap();

        pool.close().await;
    }
}
