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

/// 目标数据库不存在时创建它（借道维护库 `postgres`，`CREATE DATABASE` 不能放在事务里）。
pub async fn ensure_database(url: &str) -> Result<()> {
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

    if found.is_some() {
        println!("数据库 {database} 已存在");
    } else {
        sqlx::raw_sql(&format!("CREATE DATABASE \"{database}\""))
            .execute(&admin)
            .await
            .with_context(|| format!("创建数据库 {database} 失败"))?;
        println!("已创建数据库 {database}");
    }

    admin.close().await;
    Ok(())
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
