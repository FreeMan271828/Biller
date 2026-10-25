//! 银行同步的业务层：算同步窗口、去重、按记忆给建议、确认后入账。
//!
//! 记忆系统的判定顺序（严格按需求）：
//! ① 店家记忆命中 → 用它（多条取最近）
//! ② 否则关键词命中 → 用它（关键词越长越具体，优先命中）
//! ③ 都没有 → 账本/分类都用「默认」
//!
//! 每次确认（包括手工改过账本/分类的）都会把结果写回店家记忆，越用越准。

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use chrono::{DateTime, NaiveDate, Utc};

use crate::bank::client::BankAccount;
use crate::bank::dao::{BankDao, KeywordRule, MemoryEntry, StoredAccount};
use crate::bank::model::{BankTransaction, SyncWindow, memo_key};
use crate::bill::model::BillEntity;
use crate::bill::service::{BillService, NewBill};
use crate::common::db::Pool;
use crate::common::types::BillId;

/// 没命中任何记忆时用的账本/分类名（不存在时账单服务会自动建）。
pub const DEFAULT_BOOK: &str = "默认";
pub const DEFAULT_CATEGORY: &str = "默认";

/// 界面上那两个按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    Today,
    Week,
}

impl SyncKind {
    pub fn label(&self) -> &'static str {
        match self {
            SyncKind::Today => "今日",
            SyncKind::Week => "本周",
        }
    }
}

/// 建议是从哪条规则来的，界面上要标出来，用户才知道为什么这么预选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionSource {
    /// 规则一：这个店家以前记过。
    Memory { hits: i64 },
    /// 规则二：命中了关键词。
    Keyword(String),
    /// 都没命中。
    Default,
}

impl SuggestionSource {
    pub fn describe(&self) -> String {
        match self {
            SuggestionSource::Memory { hits } => format!("记忆({hits}次)"),
            SuggestionSource::Keyword(word) => format!("关键词「{word}」"),
            SuggestionSource::Default => "默认".to_string(),
        }
    }
}

/// 对一条流水的记账建议（用名称表达，交给账单服务去解析/自动建）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub book: String,
    pub category: String,
    pub source: SuggestionSource,
}

/// 预置关键词：只补缺，不覆盖用户改过的。
///
/// 分类名优先匹配到子分类（比如「夜宵」），找不到再退到根分类。
pub const DEFAULT_KEYWORDS: [(&str, &str); 12] = [
    ("正新鸡排", "夜宵"),
    ("鸡排", "夜宵"),
    ("汉堡", "夜宵"),
    ("咖啡", "下午茶"),
    ("奶茶", "下午茶"),
    ("水果", "水果"),
    ("打车", "交通"),
    ("地铁", "交通"),
    ("公交", "交通"),
    ("医院", "医疗"),
    ("药房", "医疗"),
    ("药品", "医疗"),
];

#[derive(Clone)]
pub struct BankService {
    pool: Pool,
    dao: BankDao,
    bills: BillService,
    categories: crate::category::service::CategoryService,
    /// 关键词规则缓存：同步时会逐条 `suggest`，每条都重查一次 `keyword_rules`
    /// 就是典型的 N+1；规则很少变，按服务实例缓存，写规则时清掉。
    keyword_cache: Arc<Mutex<Option<Vec<KeywordRule>>>>,
}

impl BankService {
    pub fn new(pool: Pool) -> Self {
        Self {
            dao: BankDao::new(pool.clone()),
            bills: BillService::new(pool.clone()),
            categories: crate::category::service::CategoryService::new(pool.clone()),
            pool,
            keyword_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// 分类树：关键词规则的目标分类从这里挑。
    pub async fn category_tree(&self) -> Result<Vec<crate::category::service::CategoryNode>> {
        self.categories.tree().await
    }

    /// 本地最新一条账单的日期，决定「本周」同步从哪天开始。
    pub async fn latest_local_bill_date(&self) -> Result<Option<NaiveDate>> {
        self.dao.latest_bill_date().await
    }

    /// 算同步窗口。
    ///
    /// 今日：[今天, 今天]；
    /// 本周：[max(本周一, 本地最新账单+1), min(今天, 本周日)]，为空时返回 `None`。
    pub async fn window(
        &self,
        kind: SyncKind,
        today: NaiveDate,
    ) -> Result<Option<SyncWindow>> {
        match kind {
            SyncKind::Today => Ok(Some(SyncWindow::today(today))),
            SyncKind::Week => {
                let latest = self.latest_local_bill_date().await?;
                Ok(SyncWindow::this_week(today, latest))
            }
        }
    }

    /// 按记忆系统给一条流水建议。
    pub async fn suggest(&self, memo: &str) -> Result<Suggestion> {
        // ① 店家记忆
        if let Some(entry) = self.dao.memory_for(memo).await? {
            return Ok(Suggestion {
                book: entry.book_name,
                category: entry.category_name,
                source: SuggestionSource::Memory { hits: entry.hits },
            });
        }

        // ② 关键词（规则已按长度降序，长的更具体）
        let haystack = memo_key(memo).to_lowercase();
        for rule in self.keyword_rules().await? {
            let keyword = rule.keyword.to_lowercase();
            if !keyword.is_empty() && haystack.contains(&keyword) {
                return Ok(Suggestion {
                    book: DEFAULT_BOOK.to_string(),
                    category: rule.category_name,
                    source: SuggestionSource::Keyword(rule.keyword),
                });
            }
        }

        // ③ 默认
        Ok(Suggestion {
            book: DEFAULT_BOOK.to_string(),
            category: DEFAULT_CATEGORY.to_string(),
            source: SuggestionSource::Default,
        })
    }

    /// 取关键词规则；同一实例内只查一次库。
    async fn keyword_rules(&self) -> Result<Vec<KeywordRule>> {
        let cached = self
            .keyword_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        if let Some(rules) = cached {
            return Ok(rules);
        }

        let rules = self.dao.keywords().await?;
        *self
            .keyword_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(rules.clone());
        Ok(rules)
    }

    /// 规则表一改，缓存立刻作废（否则同一实例后续建议会用到旧规则）。
    fn invalidate_keyword_cache(&self) {
        *self
            .keyword_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// 过滤掉已经导入过的流水（按指纹），保持原有顺序。
    pub async fn new_transactions(
        &self,
        transactions: Vec<BankTransaction>,
    ) -> Result<Vec<BankTransaction>> {
        if transactions.is_empty() {
            return Ok(Vec::new());
        }

        let fingerprints: Vec<String> = transactions
            .iter()
            .map(|txn| txn.fingerprint.clone())
            .collect();
        let imported = self.dao.imported(&fingerprints).await?;

        Ok(transactions
            .into_iter()
            .filter(|txn| !imported.contains(&txn.fingerprint))
            .collect())
    }

    /// 确认一条流水：记一笔账单 → 标记已导入 → 学会这条记忆。
    ///
    /// 返回落库后的账单，调用方用它刷新界面。
    ///
    /// 幂等性有两道保障：
    ///   * 指纹已经导入过就直接拒绝，不会给同一笔流水记第二笔账；
    ///   * 三处写入在**同一个事务**里提交，「记了账但没标记」这种半成品状态不存在
    ///     （否则下次同步会把它当成新流水再记一遍）。
    pub async fn confirm(
        &self,
        transaction: &BankTransaction,
        book: &str,
        category: &str,
    ) -> Result<BillEntity> {
        if !self
            .dao
            .imported(std::slice::from_ref(&transaction.fingerprint))
            .await?
            .is_empty()
        {
            bail!(
                "这笔流水已经导入过了（指纹 {}），不重复记账",
                transaction.fingerprint
            );
        }

        let input = NewBill {
            kind: transaction.kind,
            amount: transaction.amount,
            book: book.trim().to_string(),
            category: category.trim().to_string(),
            // 银行流水是北京时间墙上时间，按 `as_utc` 存，日期才不会偏
            created_at: transaction.as_utc(),
            // 备注统一走归一化，避免把 memo 的尾部空格带进账单
            remark: Some(crate::bank::model::normalize_memo(&transaction.memo))
                .filter(|text| !text.is_empty()),
        };

        // 账本/分类的「取或建」放在事务外：它们幂等，先建好再开事务更稳妥
        let (book, category, _auto_created) = self.bills.prepare_entry(&input).await?;

        let mut tx = self.pool.any().begin().await?;
        let bill = self
            .bills
            .insert_prepared(&mut tx, &input, book, category)
            .await?;
        // 标记已导入：带上 bill_id，方便以后追溯；账单被删也不会级联删掉这条记录
        self.dao
            .mark_imported_in(&mut tx, transaction, Some(bill.id as BillId))
            .await?;
        // 学习：这个店家以后默认记到这个账本 + 分类
        self.dao
            .remember_in(&mut tx, &transaction.memo, bill.book.id, bill.category.id)
            .await?;
        tx.commit().await?;

        // 「上次使用的日期/账本」失败不影响已经落库的账单，所以放在提交之后补记
        self.bills
            .remember_entry_date(bill.created_at.date_naive())
            .await?;
        self.bills.remember_entry_book(&bill.book.name).await?;

        Ok(bill)
    }

    /// 预置关键词规则：只补缺，返回新插入的条数。
    pub async fn seed_default_keywords(&self) -> Result<u64> {
        let mut inserted = 0;
        for (keyword, category_name) in DEFAULT_KEYWORDS {
            inserted += self.dao.seed_keyword(keyword, category_name).await?;
        }
        self.invalidate_keyword_cache();
        Ok(inserted)
    }

    // ---------------- 透传给界面用 ----------------

    pub async fn stored_accounts(&self) -> Result<Vec<StoredAccount>> {
        self.dao.accounts().await
    }

    /// 保存账户并把它设为**唯一**默认账户。
    ///
    /// 之前只写了 `is_default = true`，换一张卡后旧账户仍是默认，
    /// 会出现两个「默认账户」，取默认时结果取决于排序。
    pub async fn save_account(&self, account: &BankAccount, alias: &str) -> Result<()> {
        self.dao.save_account(account, alias, true).await?;
        self.dao.set_default_account(&account.account_uid).await
    }

    pub async fn keywords(&self) -> Result<Vec<KeywordRule>> {
        self.dao.keywords().await
    }

    pub async fn set_keyword(
        &self,
        keyword: &str,
        category_id: crate::common::types::CategoryId,
    ) -> Result<()> {
        self.dao.set_keyword(keyword, category_id).await?;
        self.invalidate_keyword_cache();
        Ok(())
    }

    pub async fn remove_keyword(&self, keyword: &str) -> Result<u64> {
        let removed = self.dao.remove_keyword(keyword).await?;
        self.invalidate_keyword_cache();
        Ok(removed)
    }

    pub async fn memory_list(&self) -> Result<Vec<MemoryEntry>> {
        self.dao.memory_list().await
    }

    pub async fn clear_memory(&self) -> Result<u64> {
        self.dao.clear_memory().await
    }

    pub async fn imported_total(&self) -> Result<i64> {
        self.dao.imported_total().await
    }

    /// 账单要用的时间：测试与界面提示都靠它，避免各处自己算。
    pub fn created_at_of(transaction: &BankTransaction) -> DateTime<Utc> {
        transaction.as_utc()
    }

    /// 给界面用的今天（UTC 日期）。
    pub fn today() -> NaiveDate {
        Utc::now().date_naive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bill::model::BillKind;
    use crate::common::db;

    use chrono::{NaiveDateTime, TimeZone};

    async fn setup() -> (Pool, BankService, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "biller-bank-svc-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let pool = db::connect(&db::sqlite_url(&dir.join("biller.db")))
            .await
            .expect("连接临时 SQLite");
        db::init_schema(&pool).await.expect("建表");

        let service = BankService::new(pool.clone());
        (pool, service, dir)
    }

    /// 建一个账本 + 「餐饮」及其子分类「夜宵」，返回 (夜宵id, 餐饮id)。
    async fn seed_categories(pool: &Pool) -> (i64, i64) {
        sqlx::query(&pool.shared("INSERT INTO bill_books (name) VALUES (?)"))
            .bind("微信")
            .execute(pool.any())
            .await
            .unwrap();

        let (food,): (i64,) = sqlx::query_as(&pool.shared(
            "INSERT INTO categories (parent_id, name) VALUES (NULL, ?) RETURNING id",
        ))
        .bind("餐饮")
        .fetch_one(pool.any())
        .await
        .unwrap();

        let (night,): (i64,) = sqlx::query_as(&pool.shared(
            "INSERT INTO categories (parent_id, name) VALUES (?, ?) RETURNING id",
        ))
        .bind(food)
        .bind("夜宵")
        .fetch_one(pool.any())
        .await
        .unwrap();

        (night, food)
    }

    fn transaction(fingerprint: &str, memo: &str, amount: i64) -> BankTransaction {
        BankTransaction {
            date_time: NaiveDate::from_ymd_opt(2026, 10, 18)
                .unwrap()
                .and_hms_opt(20, 38, 3)
                .unwrap(),
            kind: BillKind::Expense,
            amount,
            memo: memo.to_string(),
            balance: None,
            raw_type: "网联协议支付".to_string(),
            fingerprint: fingerprint.to_string(),
        }
    }

    // ---------- 窗口 ----------

    #[tokio::test]
    async fn today_window_needs_no_local_state() {
        let (pool, service, dir) = setup().await;

        let window = service
            .window(SyncKind::Today, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap())
            .await
            .unwrap()
            .expect("今日总有窗口");

        assert_eq!(window.start_text(), "20261018");
        assert_eq!(window.end_text(), "20261018");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn week_window_uses_the_latest_local_bill() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        // 空库：从本周一开始
        let window = service
            .window(SyncKind::Week, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap())
            .await
            .unwrap()
            .expect("应有窗口");
        assert_eq!(window.start_text(), "20261012");

        // 记一笔 10-15 的账单后，起点应变成 10-16
        let mut txn = transaction("fp-week", "本地已存在", 100);
        txn.date_time = NaiveDate::from_ymd_opt(2026, 10, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        service.confirm(&txn, "微信", "餐饮").await.unwrap();

        let window = service
            .window(SyncKind::Week, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap())
            .await
            .unwrap()
            .expect("应有窗口");
        assert_eq!(window.start_text(), "20261016", "应跳过已经记过的日子");

        // 本地已经记到今天 → 本周没有要同步的
        let mut txn = transaction("fp-today", "今天", 100);
        txn.date_time = NaiveDate::from_ymd_opt(2026, 10, 18)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        service.confirm(&txn, "微信", "餐饮").await.unwrap();

        assert!(
            service
                .window(SyncKind::Week, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap())
                .await
                .unwrap()
                .is_none(),
            "本地已记到最新时不应再有窗口"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 记忆系统 ----------

    #[tokio::test]
    async fn suggestion_falls_back_to_default() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        let suggestion = service.suggest("某个没见过的店家").await.unwrap();
        assert_eq!(suggestion.book, DEFAULT_BOOK);
        assert_eq!(suggestion.category, DEFAULT_CATEGORY);
        assert_eq!(suggestion.source, SuggestionSource::Default);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keyword_rules_are_used_before_at_default() {
        let (pool, service, dir) = setup().await;
        let (night, _food) = seed_categories(&pool).await;

        // 预置关键词会挑到子分类「夜宵」
        service.seed_default_keywords().await.unwrap();

        let suggestion = service
            .suggest("网银在线-京东商城-正新鸡排 ")
            .await
            .unwrap();
        assert_eq!(suggestion.category, "夜宵", "鸡排应命中夜宵");
        assert_eq!(
            suggestion.source,
            SuggestionSource::Keyword("正新鸡排".to_string()),
            "关键词越长越具体，应命中它"
        );

        // 生效的规则确实指向子分类
        let rules = service.keywords().await.unwrap();
        let rule = rules
            .iter()
            .find(|rule| rule.keyword == "正新鸡排")
            .expect("应有这条规则");
        assert_eq!(rule.category_id, night as crate::common::types::CategoryId);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merchant_memory_wins_over_keywords() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;
        service.seed_default_keywords().await.unwrap();

        // 先确认一笔「正新鸡排」，把它记到 微信/餐饮（而不是关键词的夜宵）
        let txn = transaction("fp-1", "网银在线-京东商城-正新鸡排 ", 1420);
        let bill = service.confirm(&txn, "微信", "餐饮").await.unwrap();
        assert_eq!(bill.category.name, "餐饮");

        // 下次同样的店家：规则一优先，应给出「微信 / 餐饮」而不是关键词的「夜宵」
        let suggestion = service.suggest("财付通-微信支付-正新鸡排").await.unwrap();
        assert_eq!(suggestion.book, "微信", "记忆应带出账本");
        assert_eq!(suggestion.category, "餐饮", "记忆优先于关键词");
        assert_eq!(
            suggestion.source,
            SuggestionSource::Memory { hits: 1 },
            "来源应标成记忆"
        );

        // 再补一笔同样的店家、不同分类：应更新映射
        let txn = transaction("fp-2", "正新鸡排", 100);
        service.confirm(&txn, "微信", "夜宵").await.unwrap();

        let suggestion = service.suggest("正新鸡排").await.unwrap();
        assert_eq!(suggestion.category, "夜宵", "应更新成最近一次的选择");
        assert_eq!(suggestion.source, SuggestionSource::Memory { hits: 2 });

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 去重 ----------

    #[tokio::test]
    async fn new_transactions_skips_imported_ones() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        let first = transaction("fp-a", "店家A", 100);
        let second = transaction("fp-b", "店家B", 200);

        // 一开始两条都是新的
        let fresh = service
            .new_transactions(vec![first.clone(), second.clone()])
            .await
            .unwrap();
        assert_eq!(fresh.len(), 2);

        service.confirm(&first, "微信", "餐饮").await.unwrap();

        // 导入过的不再出现，顺序保持不变
        let fresh = service
            .new_transactions(vec![first.clone(), second.clone()])
            .await
            .unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].fingerprint, "fp-b");

        // 空输入直接返回空
        assert!(service.new_transactions(Vec::new()).await.unwrap().is_empty());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 入账 ----------

    #[tokio::test]
    async fn confirm_creates_bill_with_the_bank_date() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        // 凌晨 01:18 的交易：日期必须还是 10-16
        let mut txn = transaction("fp-night", "财付通-微信支付-Apple ", 3400);
        txn.date_time = NaiveDate::from_ymd_opt(2026, 10, 16)
            .unwrap()
            .and_hms_opt(1, 18, 48)
            .unwrap();

        let bill = service.confirm(&txn, "微信", "餐饮").await.unwrap();

        assert_eq!(bill.kind, BillKind::Expense);
        assert_eq!(bill.amount, 3400);
        assert_eq!(bill.created_at.date_naive(), NaiveDate::from_ymd_opt(2026, 10, 16).unwrap());
        assert_eq!(bill.book.name, "微信");
        assert_eq!(bill.category.name, "餐饮");
        assert_eq!(bill.remark.as_deref(), Some("财付通-微信支付-Apple"));

        // 已导入记录也带上了账单 id
        assert_eq!(service.imported_total().await.unwrap(), 1);
        let (bill_id,): (Option<i64>,) = sqlx::query_as(&pool.shared(
            "SELECT bill_id FROM bank_imports WHERE fingerprint = ?",
        ))
        .bind("fp-night")
        .fetch_one(pool.any())
        .await
        .unwrap();
        assert_eq!(bill_id, Some(bill.id as i64));

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn confirm_autocreates_unknown_book_and_category() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        // 「默认」账本/分类一开始不存在，账单服务应自动建
        let txn = transaction("fp-default", "没见过的店", 500);
        let bill = service
            .confirm(&txn, DEFAULT_BOOK, DEFAULT_CATEGORY)
            .await
            .unwrap();

        assert_eq!(bill.book.name, DEFAULT_BOOK);
        assert_eq!(bill.category.name, DEFAULT_CATEGORY);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn income_transactions_are_recorded_as_income() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        let mut txn = transaction("fp-income", "支付宝-退款", 7900);
        txn.kind = BillKind::Income;

        let bill = service.confirm(&txn, "微信", "餐饮").await.unwrap();
        assert_eq!(bill.kind, BillKind::Income);
        assert_eq!(bill.amount, 7900);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn created_at_helper_matches_the_bank_wall_clock() {
        let txn = transaction("fp", "x", 100);
        let created = BankService::created_at_of(&txn);
        let expected: NaiveDateTime = txn.date_time;

        assert_eq!(created.naive_utc(), expected);
        // 也确认一下 Utc 类型没被时区搞乱
        assert_eq!(
            Utc.from_utc_datetime(&expected).timestamp(),
            created.timestamp()
        );
    }

    /// 同一笔流水确认两次只能入账一次：之前第二次会再插一笔账单。
    #[tokio::test]
    async fn confirming_the_same_transaction_twice_is_rejected() {
        let (pool, service, dir) = setup().await;
        seed_categories(&pool).await;

        let txn = transaction("fp-dup", "重复店家", 1500);
        let bill = service.confirm(&txn, "微信", "餐饮").await.unwrap();

        let error = service
            .confirm(&txn, "微信", "餐饮")
            .await
            .expect_err("同一指纹的第二次确认必须被拒绝");
        assert!(
            error.to_string().contains("已经导入过"),
            "错误提示应说明重复导入：{error}"
        );

        // 账单和导入记录都只能有一条
        let (bill_count,): (i64,) = sqlx::query_as(&pool.shared("SELECT COUNT(*) FROM bills"))
            .fetch_one(pool.any())
            .await
            .unwrap();
        assert_eq!(bill_count, 1, "重复确认不能多记一笔账");
        assert_eq!(service.imported_total().await.unwrap(), 1);

        let (bill_id,): (Option<i64>,) = sqlx::query_as(&pool.shared(
            "SELECT bill_id FROM bank_imports WHERE fingerprint = ?",
        ))
        .bind("fp-dup")
        .fetch_one(pool.any())
        .await
        .unwrap();
        assert_eq!(bill_id, Some(bill.id as i64));

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保存第二个账户后，默认账户必须仍然只有一个（且是刚保存的那个）。
    #[tokio::test]
    async fn saving_an_account_keeps_exactly_one_default() {
        let (pool, service, dir) = setup().await;

        let first = BankAccount {
            account_uid: "P-1".to_string(),
            sub_account_no: "111".to_string(),
        };
        let second = BankAccount {
            account_uid: "P-2".to_string(),
            sub_account_no: "222".to_string(),
        };

        service.save_account(&first, "工资卡").await.unwrap();
        service.save_account(&second, "主力卡").await.unwrap();

        let accounts = service.stored_accounts().await.unwrap();
        let defaults: Vec<&StoredAccount> = accounts.iter().filter(|item| item.is_default).collect();
        assert_eq!(defaults.len(), 1, "默认账户必须唯一：{accounts:?}");
        assert_eq!(defaults[0].account.account_uid, "P-2");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关键词缓存不能盖住刚写入的新规则。
    #[tokio::test]
    async fn keyword_cache_is_invalidated_by_writes() {
        let (pool, service, dir) = setup().await;
        let (night, _food) = seed_categories(&pool).await;

        // 先读一次，把空规则缓存起来
        assert_eq!(service.suggest("正新鸡排").await.unwrap().source, SuggestionSource::Default);

        service
            .set_keyword("正新鸡排", night as crate::common::types::CategoryId)
            .await
            .unwrap();
        let suggestion = service.suggest("正新鸡排").await.unwrap();
        assert_eq!(suggestion.category, "夜宵", "写入规则后缓存必须失效");

        service.remove_keyword("正新鸡排").await.unwrap();
        assert_eq!(
            service.suggest("正新鸡排").await.unwrap().source,
            SuggestionSource::Default,
            "删掉规则后缓存也必须失效"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
