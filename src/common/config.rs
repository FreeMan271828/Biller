//! 本地配置：记住「用哪个库」。
//!
//! 存放在 `%USERPROFILE%\.biller\config.toml`：
//!
//! ```toml
//! # mode 取 local（本地 SQLite 文件）或 postgres（远程库）
//! mode = "local"
//! sqlite_path = "C:\\Users\\me\\.biller\\biller.db"
//! postgres_url = "postgres://postgres:postgres@127.0.0.1:5432/biller"
//! ```
//!
//! 连接串优先级（后者被前者覆盖）：
//! 内置默认（**本地 SQLite**） < 配置文件 < `DATABASE_URL` 环境变量 < `--database-url`。
//!
//! 故意手写这个极小的 TOML 子集：只认 `key = "value"` 与 `#` 注释，
//! 不值得为此引入 `toml` + `serde` 两个依赖。

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::common::db;

/// 数据库模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbMode {
    /// 本地 SQLite 文件（默认，零配置）
    Local,
    /// 远程 PostgreSQL
    Postgres,
}

impl DbMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DbMode::Local => "local",
            DbMode::Postgres => "postgres",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" | "sqlite" | "本地" => Some(DbMode::Local),
            "postgres" | "postgresql" | "pg" | "远程" => Some(DbMode::Postgres),
            _ => None,
        }
    }

    /// 界面上的名字。
    pub fn label(&self) -> &'static str {
        match self {
            DbMode::Local => "本地文件（SQLite）",
            DbMode::Postgres => "PostgreSQL",
        }
    }

    pub fn next(self) -> Self {
        match self {
            DbMode::Local => DbMode::Postgres,
            DbMode::Postgres => DbMode::Local,
        }
    }
}

/// 应用配置。
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub mode: DbMode,
    /// 本地模式的库文件路径。
    pub sqlite_path: PathBuf,
    /// 远程模式的连接串。
    pub postgres_url: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // 默认走本地：第一次运行就能用，不需要先装一个 PostgreSQL
            mode: DbMode::Local,
            sqlite_path: db::default_sqlite_path(),
            postgres_url: "postgres://postgres:postgres@127.0.0.1:5432/biller".to_string(),
        }
    }
}

impl AppConfig {
    /// 配置文件路径：`%USERPROFILE%\.biller\config.toml`。
    pub fn path() -> PathBuf {
        db::default_sqlite_path().with_file_name("config.toml")
    }

    /// 读配置；文件不存在就用默认值（本地 SQLite）。
    pub fn load() -> Result<Self> {
        let path = Self::path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };

        let mut config = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            let value = unquote(value.trim());
            match key.trim() {
                "mode" => config.mode = DbMode::parse(&value).unwrap_or(config.mode),
                "sqlite_path" if !value.is_empty() => config.sqlite_path = PathBuf::from(value),
                "postgres_url" if !value.is_empty() => config.postgres_url = value,
                _ => {}
            }
        }

        Ok(config)
    }

    /// 写回配置文件（目录不存在时自动创建）。
    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }

        let text = format!(
            "# Biller 配置；mode 取 local（本地 SQLite 文件）或 postgres（远程库）\n\
             mode = \"{}\"\n\
             sqlite_path = \"{}\"\n\
             postgres_url = \"{}\"\n",
            self.mode.as_str(),
            escape(&self.sqlite_path.to_string_lossy()),
            escape(&self.postgres_url),
        );

        std::fs::write(&path, text).with_context(|| format!("写入 {} 失败", path.display()))?;
        Ok(())
    }

    /// 当前配置对应的连接串。
    pub fn url(&self) -> String {
        match self.mode {
            DbMode::Local => db::sqlite_url(&self.sqlite_path),
            DbMode::Postgres => self.postgres_url.clone(),
        }
    }

    /// 决定最终连接串：`--database-url` > `DATABASE_URL` > 配置文件。
    pub fn resolve_url(&self, cli_value: Option<String>) -> String {
        if let Some(url) = cli_value.filter(|value| !value.trim().is_empty()) {
            return url;
        }
        if let Ok(url) = std::env::var("DATABASE_URL") {
            if !url.trim().is_empty() {
                return url;
            }
        }
        self.url()
    }
}

/// 去掉两侧引号，并把转义过的反斜杠还原。
fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    let stripped = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);

    stripped.replace("\\\\", "\\")
}

/// 写进配置前转义反斜杠与引号（Windows 路径必须有这一步）。
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_unescapes_windows_paths() {
        // 转义 → 还原必须完全一致，否则 Windows 路径会被吃掉反斜杠
        let path = r"C:\Users\me\.biller\biller.db";
        assert_eq!(unquote(&format!("\"{}\"", escape(path))), path);
        assert_eq!(escape(r"a\b"), r"a\\b");
    }

    #[test]
    fn unquote_tolerates_bare_values() {
        assert_eq!(unquote("local"), "local");
        assert_eq!(unquote("\"local\""), "local");
    }

    #[test]
    fn parses_modes() {
        assert_eq!(DbMode::parse("local"), Some(DbMode::Local));
        assert_eq!(DbMode::parse("SQLite"), Some(DbMode::Local));
        assert_eq!(DbMode::parse("postgres"), Some(DbMode::Postgres));
        assert_eq!(DbMode::parse("本地"), Some(DbMode::Local));
        assert_eq!(DbMode::parse("未知"), None);
        assert_eq!(DbMode::Local.next(), DbMode::Postgres);
    }

    #[test]
    fn default_is_local_with_a_path_under_user_profile() {
        let config = AppConfig::default();
        assert_eq!(config.mode, DbMode::Local);
        assert!(
            config.sqlite_path.ends_with("biller.db"),
            "默认库文件应叫 biller.db：{}",
            config.sqlite_path.display()
        );
        assert!(config.url().starts_with("sqlite:"));
        assert_eq!(AppConfig::path().file_name().unwrap(), "config.toml");
    }
}
