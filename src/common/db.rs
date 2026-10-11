//! 数据库公共服务：连接串解析、连接池、建库、建表、错误映射。
//!
//! 各领域的 `dao` 只依赖这里给出的连接池，不关心连接从哪来。

use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sqlx::AnyPool;
use sqlx::any::AnyPoolOptions;

/// 未配置连接串时使用的默认值（本地开发）。
const DEFAULT_DATABASE_URL: &str = "postgres://postgres:postgres@127.0.0.1:5432/biller";

/// 建表语句，随二进制一起编译；按后端各一份。
const SCHEMA_POSTGRES_SQL: &str = include_str!("schema.sql");
const SCHEMA_SQLITE_SQL: &str = include_str!("schema_sqlite.sql");

/// 后端类型：SQL 文本要按它分叉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Postgres,
    Sqlite,
}

impl DbKind {
    /// 从连接串推断后端。
    pub fn from_url(url: &str) -> Self {
        if url.trim_start().to_ascii_lowercase().starts_with("sqlite:") {
            DbKind::Sqlite
        } else {
            DbKind::Postgres
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::Sqlite => "本地 SQLite",
        }
    }
}

/// 连接池 + 后端类型。
///
/// 两个后端都走 `sqlx::Any`：占位符统一 `?`，**bind 链只写一遍**，
/// 只有 SQL 文本按 [`DbKind`] 分叉。
///
/// 注意：`Any` **不支持 chrono 类型**（`Encode<'_, Any>` 没实现），所以
/// 时间戳 / 日期一律以 ISO 文本往返，各 dao 里负责字符串 ↔ chrono 的转换。
#[derive(Clone)]
pub struct Pool {
    inner: AnyPool,
    kind: DbKind,
}

impl Pool {
    pub fn new(inner: AnyPool, kind: DbKind) -> Self {
        Self { inner, kind }
    }

    /// 交给 sqlx 执行的原始池。
    pub fn any(&self) -> &AnyPool {
        &self.inner
    }

    pub fn kind(&self) -> DbKind {
        self.kind
    }

    /// 把 `?` 占位符改写成 PostgreSQL 的 `$n`（SQLite 原样返回）。
    ///
    /// **`sqlx::Any` 不做这件事**：PostgreSQL 只认 `$n`，SQLite 只认 `?`
    /// （实测过：把 `?` 直接发给 PG 会报 `syntax error at end of input`）。
    /// 所以全项目的 SQL 统一写成 `?`，发起前按后端改写。
    pub fn placeholders(&self, query: &str) -> String {
        match self.kind {
            DbKind::Sqlite => query.to_string(),
            DbKind::Postgres => {
                let mut index = 0;
                let mut out = String::with_capacity(query.len() + 8);

                for character in query.chars() {
                    if character == '?' {
                        index += 1;
                        out.push('$');
                        out.push_str(&index.to_string());
                    } else {
                        out.push(character);
                    }
                }

                out
            }
        }
    }

    /// 两个后端同一条 SQL（只差占位符写法）。
    pub fn shared(&self, query: &str) -> String {
        self.placeholders(query)
    }

    /// 按后端挑一条 SQL，并改写占位符 —— 分叉点集中在这里。
    pub fn sql(&self, postgres: &str, sqlite: &str) -> String {
        match self.kind {
            DbKind::Postgres => self.placeholders(postgres),
            DbKind::Sqlite => self.placeholders(sqlite),
        }
    }

    pub async fn close(&self) {
        self.inner.close().await;
    }
}

/// `Any` 的驱动表是全局一次性的，注册多次无害但没必要。
fn install_drivers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(sqlx::any::install_default_drivers);
}

/// 本地模式的默认库文件：`%USERPROFILE%\.biller\biller.db`。
pub fn default_sqlite_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    home.join(".biller").join("biller.db")
}

/// 把本地文件路径变成 SQLite 连接串（统一用正斜杠）。
///
/// `mode=rwc` 是必需的：sqlx 的 SQLite 默认 `create_if_missing = false`，
/// 不带这个参数首次运行会直接报 `unable to open database file (code 14)`。
pub fn sqlite_url(path: &FsPath) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    format!("sqlite:{text}?mode=rwc")
}

/// 从 `sqlite:` 连接串里取出文件路径；内存库返回 `None`。
pub fn sqlite_file_path(url: &str) -> Option<PathBuf> {
    let rest = url.trim().strip_prefix("sqlite:")?;
    let rest = rest.split('?').next().unwrap_or(rest);

    if rest.is_empty() || rest.contains(":memory:") {
        return None;
    }

    // `sqlite:foo.db`、`sqlite://foo.db`、`sqlite:///C:/foo.db` 都要吃
    let path = rest.trim_start_matches("//");
    let path = if path.len() > 2 && path.starts_with('/') && path.as_bytes()[2] == b':' {
        &path[1..]
    } else {
        path
    };

    Some(PathBuf::from(path))
}

/// 建 SQLite 文件所在的目录（文件本身由连接时创建）。
fn ensure_sqlite_parent(url: &str) -> Result<()> {
    let Some(path) = sqlite_file_path(url) else {
        return Ok(());
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
    }
    Ok(())
}

/// 解析连接串：命令行参数 > 环境变量 `DATABASE_URL` > 内置默认值。
pub fn resolve_url(cli_value: Option<String>) -> Result<String> {
    if let Some(url) = cli_value.filter(|value| !value.trim().is_empty()) {
        return Ok(url);
    }
    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.trim().is_empty() {
            return Ok(url);
        }
    }
    Ok(DEFAULT_DATABASE_URL.to_string())
}

/// 建立连接池（PostgreSQL 或本地 SQLite，看连接串前缀）。
pub async fn connect(url: &str) -> Result<Pool> {
    install_drivers();

    let kind = DbKind::from_url(url);
    // 本地模式顺手把目录建出来：用户随手填个新路径也应该能用
    if kind == DbKind::Sqlite {
        ensure_sqlite_parent(url)?;
    }

    let pool = AnyPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
        .with_context(|| {
            format!(
                "连接{}失败：{}\n请确认数据库可用，或用 --database-url 指定连接串",
                kind.label(),
                redact(url)
            )
        })?;

    Ok(Pool::new(pool, kind))
}

/// 建库结果。调用方决定怎么提示——TUI 里不能直接 `println!`，会污染界面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseOutcome {
    Created,
    Existed,
}

/// 目标数据库不存在时创建它（借道维护库 `postgres`，`CREATE DATABASE` 不能放在事务里）。
///
/// 注意：这里**不打印任何东西**，提示交给调用方。
pub async fn ensure_database(url: &str) -> Result<DatabaseOutcome> {
    // 本地模式：库就是那个文件，保证父目录存在即可，文件由连接时创建
    if DbKind::from_url(url) == DbKind::Sqlite {
        ensure_sqlite_parent(url)?;
        return Ok(DatabaseOutcome::Existed);
    }

    let database = database_name(url)?;
    if !is_safe_identifier(&database) {
        bail!("数据库名只允许字母、数字和下划线，且不能以数字开头：{database}");
    }

    install_drivers();

    let admin_url = with_database(url, "postgres")?;
    let admin = AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&admin_url)
        .await
        .with_context(|| format!("连接维护库失败：{}", redact(&admin_url)))?;

    // 这条只跑在 PostgreSQL 上，直接用 `$1`
    let found: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(&database)
        .fetch_optional(&admin)
        .await
        .context("查询 pg_database 失败")?;

    let outcome = if found.is_some() {
        DatabaseOutcome::Existed
    } else {
        sqlx::raw_sql(&format!("CREATE DATABASE \"{database}\""))
            .execute(&admin)
            .await
            .with_context(|| format!("创建数据库 {database} 失败"))?;
        DatabaseOutcome::Created
    };

    admin.close().await;
    Ok(outcome)
}

/// 执行建表语句（幂等，可重复运行）；按后端选对应的 schema。
pub async fn init_schema(pool: &Pool) -> Result<()> {
    let schema = match pool.kind() {
        DbKind::Postgres => SCHEMA_POSTGRES_SQL,
        DbKind::Sqlite => SCHEMA_SQLITE_SQL,
    };

    sqlx::raw_sql(schema)
        .execute(pool.any())
        .await
        .with_context(|| format!("初始化{}表结构失败", pool.kind().label()))?;
    Ok(())
}

/// 隐去连接串中的密码，便于安全打印。
pub fn redact(url: &str) -> String {
    let (Some(scheme_end), Some(at)) = (url.find("://"), url.rfind('@')) else {
        return url.to_string();
    };
    let credentials = &url[scheme_end + 3..at];
    match credentials.split_once(':') {
        Some((user, _)) => format!("{}://{}:***@{}", &url[..scheme_end], user, &url[at + 1..]),
        None => url.to_string(),
    }
}

/// 连接的各个组成部分：设置页编辑的就是它。
///
/// 用结构化字段而不是整串连接串，方便逐项校验（端口范围等）与展示（密码打码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: "postgres".to_string(),
            database: "biller".to_string(),
        }
    }
}

impl ConnectionSettings {
    /// 从 `postgres://user:pass@host:port/db` 解析；解析不了的部分回落到默认值。
    pub fn from_url(url: &str) -> Self {
        let mut settings = Self::default();
        let Some(rest) = url.split_once("://").map(|(_, rest)| rest) else {
            return settings;
        };

        // 查询参数（sslmode 之类）不参与设置项
        let rest = rest.split('?').next().unwrap_or(rest);
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, Some(path)),
            None => (rest, None),
        };

        let (credentials, host_port) = match authority.rsplit_once('@') {
            Some((credentials, host_port)) => (Some(credentials), host_port),
            None => (None, authority),
        };

        if let Some(credentials) = credentials {
            let (user, password) = match credentials.split_once(':') {
                Some((user, password)) => (user, password),
                None => (credentials, ""),
            };
            settings.user = decode_component(user);
            settings.password = decode_component(password);
        }

        match host_port.rsplit_once(':') {
            Some((host, port)) => {
                settings.host = host.to_string();
                if let Ok(port) = port.parse() {
                    settings.port = port;
                }
            }
            None => settings.host = host_port.to_string(),
        }

        if let Some(path) = path.filter(|path| !path.is_empty()) {
            settings.database = path.to_string();
        }

        settings
    }

    /// 拼回连接串；用户名与密码会按 URL 规则转义（密码里带 `@ : /` 也不会拼坏）。
    pub fn to_url(&self) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}",
            encode_component(&self.user),
            encode_component(&self.password),
            self.host,
            self.port,
            self.database
        )
    }

    /// 展示用：密码只显示星号。
    pub fn masked_password(&self) -> String {
        if self.password.is_empty() {
            "(空)".to_string()
        } else {
            "*".repeat(self.password.chars().count().min(10))
        }
    }
}

/// 只保留 URL 里的 unreserved 字符，其余按 `%XX` 转义。
fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// [`encode_component`] 的逆运算。
fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            // 用字节切片再转字符串，避免在多字节字符中间切开 &str
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&out).to_string()
}

/// 把连接串写进 `.env`（保留文件里的其它行），返回写入的路径。
///
/// 设置页保存时用；主程序启动时会用它覆盖 shell 里可能存在的同名环境变量，
/// 因此「应用里保存的设置」才是最终生效的那个。
pub fn save_database_url(url: &str) -> Result<std::path::PathBuf> {
    use std::path::PathBuf;

    let path = PathBuf::from(".env");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .map(|text| text.lines().map(|line| line.to_string()).collect())
        .unwrap_or_default();

    let entry = format!("DATABASE_URL={url}");
    let mut replaced = false;
    for line in lines.iter_mut() {
        if line.trim_start().starts_with("DATABASE_URL=") {
            *line = entry.clone();
            replaced = true;
        }
    }
    if !replaced {
        lines.push(entry);
    }

    let mut content = lines.join("\n");
    content.push('\n');
    std::fs::write(&path, content).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(path)
}

/// 从连接串中取出数据库名。
pub fn database_name(url: &str) -> Result<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .context("DATABASE_URL 缺少 scheme://，例如 postgres://user:pass@127.0.0.1:5432/biller")?;
    let without_query = rest.split('?').next().unwrap_or(rest);
    let without_fragment = without_query.split('#').next().unwrap_or(without_query);
    let (_, name) = without_fragment.rsplit_once('/').context(
        "DATABASE_URL 里没有数据库名，例如 postgres://user:pass@127.0.0.1:5432/biller",
    )?;
    if name.is_empty() {
        bail!("DATABASE_URL 里没有数据库名，例如 postgres://user:pass@127.0.0.1:5432/biller");
    }
    Ok(name.to_string())
}

/// 把连接串里的数据库名替换成另一个（用于连维护库）。
pub fn with_database(url: &str, database: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .context("DATABASE_URL 缺少 scheme://，例如 postgres://user:pass@127.0.0.1:5432/biller")?;
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    let base = match path.rsplit_once('/') {
        Some((base, _)) => base,
        None => bail!("DATABASE_URL 里没有数据库名，例如 postgres://user:pass@127.0.0.1:5432/biller"),
    };

    let mut result = format!("{scheme}://{base}/{database}");
    if let Some(query) = query {
        result.push('?');
        result.push_str(query);
    }
    Ok(result)
}

/// 外键冲突：PostgreSQL 报 SQLSTATE `23503`，SQLite 报扩展码 787 / 消息文本。
pub fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => {
            db.code().as_deref() == Some("23503")
                || db.message().to_ascii_lowercase().contains("foreign key")
        }
        _ => false,
    }
}

/// 唯一约束冲突：PostgreSQL 报 SQLSTATE `23505`，SQLite 报扩展码 2067 / 消息文本。
pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => {
            matches!(db.code().as_deref(), Some("23505") | Some("2067"))
                || db.message().to_ascii_lowercase().contains("unique constraint")
        }
        _ => false,
    }
}

/// 把约束冲突翻译成可读提示，其余错误保留原始信息并附上提示。
///
/// 参数同时接受 `sqlx::Error` 和 `anyhow::Error`：dao 层把错误转成了 anyhow，
/// 因此这里沿错误链把底层的 sqlx 错误找回来判断约束类型。
pub fn describe(error: impl Into<anyhow::Error>, hint: &str) -> anyhow::Error {
    let error = error.into();
    let is_constraint_violation = error.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .is_some_and(|sqlx_error| {
                is_foreign_key_violation(sqlx_error) || is_unique_violation(sqlx_error)
            })
    });

    if is_constraint_violation {
        anyhow!("{hint}")
    } else {
        error.context(hint.to_string())
    }
}

/// 只允许安全标识符，避免 `CREATE DATABASE` 的字符串拼接被注入。
fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_database_name() {
        assert_eq!(database_name("postgres://u:p@127.0.0.1:5432/biller").unwrap(), "biller");
        assert_eq!(database_name("postgres://u:p@h:5432/biller?sslmode=disable").unwrap(), "biller");
        assert!(database_name("postgres://u:p@127.0.0.1:5432").is_err());
    }

    #[test]
    fn swaps_database_name() {
        assert_eq!(
            with_database("postgres://u:p@127.0.0.1:5432/biller", "postgres").unwrap(),
            "postgres://u:p@127.0.0.1:5432/postgres"
        );
        assert_eq!(
            with_database("postgres://u:p@h:5432/biller?sslmode=disable", "postgres").unwrap(),
            "postgres://u:p@h:5432/postgres?sslmode=disable"
        );
    }

    #[test]
    fn hides_password() {
        assert_eq!(redact("postgres://u:secret@h:5432/db"), "postgres://u:***@h:5432/db");
        assert_eq!(redact("postgres://h:5432/db"), "postgres://h:5432/db");
    }

    #[test]
    fn validates_identifiers() {
        assert!(is_safe_identifier("biller"));
        assert!(is_safe_identifier("biller_2"));
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("2biller"));
        assert!(!is_safe_identifier("biller; DROP DATABASE x"));
    }

    #[test]
    fn parses_and_rebuilds_urls() {
        let settings = ConnectionSettings::from_url("postgres://alice:s3cr3t@10.0.0.5:6543/mydb");
        assert_eq!(settings.host, "10.0.0.5");
        assert_eq!(settings.port, 6543);
        assert_eq!(settings.user, "alice");
        assert_eq!(settings.password, "s3cr3t");
        assert_eq!(settings.database, "mydb");
        assert_eq!(
            settings.to_url(),
            "postgres://alice:s3cr3t@10.0.0.5:6543/mydb"
        );

        // 查询参数不影响设置项
        let with_query = ConnectionSettings::from_url("postgres://u:p@h:5432/db?sslmode=disable");
        assert_eq!(with_query.database, "db");
    }

    #[test]
    fn escapes_special_characters_in_credentials() {
        let settings = ConnectionSettings {
            host: "127.0.0.1".to_string(),
            port: 5432,
            user: "user@corp".to_string(),
            password: "p@ss:w/rd".to_string(),
            database: "biller".to_string(),
        };

        let url = settings.to_url();
        assert!(url.contains("%40"), "URL 里的 @ 必须转义：{url}");
        assert!(url.contains("%2F"), "URL 里的 / 必须转义：{url}");
        assert_eq!(
            ConnectionSettings::from_url(&url),
            settings,
            "转义后应能原样往返"
        );
    }

    #[test]
    fn falls_back_when_url_is_unparsable() {
        let settings = ConnectionSettings::from_url("这不是连接串");
        assert_eq!(settings, ConnectionSettings::default());
    }

    #[test]
    fn masks_password_for_display() {
        let settings = ConnectionSettings::from_url("postgres://u:secret@h:5432/db");
        assert_eq!(settings.masked_password(), "******");

        let empty = ConnectionSettings::from_url("postgres://u:@h:5432/db");
        assert_eq!(empty.masked_password(), "(空)");
    }

    /// 需要真实数据库：只有设置 BILLER_TEST_DATABASE_URL 时才会执行。
    ///
    /// 故意制造一次外键冲突，验证 SQLSTATE 翻译成了单行友好提示。
    #[tokio::test]
    async fn translates_constraint_violations() {
        let Ok(url) = std::env::var("BILLER_TEST_DATABASE_URL") else {
            return;
        };
        let pool = connect(&url).await.expect("连接测试库");
        init_schema(&pool).await.expect("初始化表结构");

        let error = sqlx::query(
            "INSERT INTO bills (kind, amount, book_id, category_id, created_at)
             VALUES ('expense', 100, 999999999, 999999999, now())",
        )
        .execute(pool.any())
        .await
        .expect_err("指向不存在的账本，应当触发外键冲突");

        assert!(
            is_foreign_key_violation(&error),
            "应被识别为外键冲突，实际错误：{error:?}"
        );

        let described = describe(error, "账本不存在，无法记账");
        assert_eq!(described.to_string(), "账本不存在，无法记账");
        assert_eq!(
            described.chain().count(),
            1,
            "友好提示不应再挂带底层原因链"
        );
    }

    /// 本地 SQLite 模式跑一遍真实流程：建表 → 记账 → 按月查询 → 起始金额 → 外键约束。
    ///
    /// 不需要任何外部数据库：临时文件，跑完就删。这是「SQLite 模式真的能用」的证明。
    #[tokio::test]
    async fn sqlite_mode_works_end_to_end() {
        use chrono::{NaiveDate, TimeZone};

        use crate::bill::dao::{BillDao, BillFilter};
        use crate::bill::model::BillKind;
        use crate::bill_book::dao::BookDao;
        use crate::category::dao::CategoryDao;
        use crate::month::dao::MonthDao;

        let dir = std::env::temp_dir().join(format!("biller-sqlite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = sqlite_url(&dir.join("biller.db"));

        let pool = connect(&url).await.expect("连接本地 SQLite");
        assert_eq!(pool.kind(), DbKind::Sqlite, "应识别成本地模式");
        assert_eq!(pool.kind().label(), "本地 SQLite");
        init_schema(&pool).await.expect("建表");

        let books = BookDao::new(pool.clone());
        let categories = CategoryDao::new(pool.clone());
        let bills = BillDao::new(pool.clone());
        let months = MonthDao::new(pool.clone());

        let book_id = books.insert("微信").await.expect("建账本");
        let category_id = categories.insert(None, "餐饮").await.expect("建分类");

        let created_at = chrono::Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        let bill_id = bills
            .insert(
                BillKind::Expense,
                1234,
                book_id,
                category_id,
                created_at,
                Some("午饭"),
            )
            .await
            .expect("记账");

        // 时间戳往返 + 月份筛选：这两条正是两个后端的方言差异所在
        let month = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let list = bills
            .list(&BillFilter {
                month: Some(month),
                ..Default::default()
            })
            .await
            .expect("按月查询");
        assert_eq!(list.len(), 1, "SQLite 下按月筛选必须能查到");
        assert_eq!(list[0].id, bill_id);
        assert_eq!(list[0].created_at, created_at, "时间戳应原样往返");
        assert_eq!(list[0].book.name, "微信");
        assert_eq!(list[0].category.name, "餐饮");

        let other = bills
            .list(&BillFilter {
                month: Some(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()),
                ..Default::default()
            })
            .await
            .expect("按月查询");
        assert!(other.is_empty(), "别的月份不该查出来");

        let by_year = bills
            .list(&BillFilter {
                year: Some(2026),
                ..Default::default()
            })
            .await
            .expect("按年查询");
        assert_eq!(by_year.len(), 1, "SQLite 下按年筛选也要能查到");

        // 每月起始金额：upsert 与读回
        months
            .set_start_balance(month, 100_000)
            .await
            .expect("写起始金额");
        assert_eq!(
            months.start_balance(month).await.expect("读起始金额"),
            100_000
        );
        assert_eq!(
            months
                .start_balance(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap())
                .await
                .expect("读起始金额"),
            0,
            "没设过的月份应为 0"
        );

        // 「计入日历」开关与更新
        bills.set_excluded(bill_id, true).await.expect("标记不计入");
        let reloaded = bills.get(bill_id).await.expect("取单条").expect("应存在");
        assert!(reloaded.excluded, "排除标记应落库");

        bills
            .update(
                bill_id,
                BillKind::Income,
                5000,
                book_id,
                category_id,
                created_at,
                None,
            )
            .await
            .expect("更新");
        let updated = bills.get(bill_id).await.expect("取单条").expect("应存在");
        assert_eq!(updated.amount, 5000);
        assert_eq!(updated.kind, BillKind::Income);
        assert!(updated.excluded, "更新不应动「计入日历」");

        // 外键约束在 SQLite 下同样生效，且要能被识别成友好提示
        let error = bills
            .insert(
                BillKind::Expense,
                1,
                999_999,
                category_id,
                created_at,
                None,
            )
            .await
            .expect_err("指向不存在的账本，应触发外键冲突");
        let sqlx_error = error
            .downcast_ref::<sqlx::Error>()
            .expect("DAO 应把 sqlx 错误原样往上抛");
        assert!(
            is_foreign_key_violation(sqlx_error),
            "SQLite 的外键错误也要能识别：{error:?}"
        );

        // 唯一约束（账本重名）
        let error = books.insert("微信").await.expect_err("重名账本应被拒绝");
        let sqlx_error = error
            .downcast_ref::<sqlx::Error>()
            .expect("DAO 应把 sqlx 错误原样往上抛");
        assert!(
            is_unique_violation(sqlx_error),
            "SQLite 的唯一约束错误也要能识别：{error:?}"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
