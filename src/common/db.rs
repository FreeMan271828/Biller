//! 数据库公共服务：连接串解析、连接池、建库、建表、错误映射。
//!
//! 各领域的 `dao` 只依赖这里给出的连接池，不关心连接从哪来。

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// 未配置连接串时使用的默认值（本地开发）。
const DEFAULT_DATABASE_URL: &str = "postgres://postgres:postgres@127.0.0.1:5432/biller";

/// 建表语句，随二进制一起编译。
const SCHEMA_SQL: &str = include_str!("schema.sql");

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

/// 建立连接池。
pub async fn connect(url: &str) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
        .with_context(|| {
            format!(
                "连接 PostgreSQL 失败：{}\n请确认数据库已启动，或用 --database-url 指定连接串",
                redact(url)
            )
        })
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
    let database = database_name(url)?;
    if !is_safe_identifier(&database) {
        bail!("数据库名只允许字母、数字和下划线，且不能以数字开头：{database}");
    }

    let admin_url = with_database(url, "postgres")?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&admin_url)
        .await
        .with_context(|| format!("连接维护库失败：{}", redact(&admin_url)))?;

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

/// 执行建表语句（幂等，可重复运行）。
pub async fn init_schema(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(SCHEMA_SQL)
        .execute(pool)
        .await
        .context("初始化表结构失败")?;
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

/// 外键冲突（PostgreSQL 23503）。
pub fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23503"))
}

/// 唯一约束冲突（PostgreSQL 23505）。
pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
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
        .execute(&pool)
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
}
