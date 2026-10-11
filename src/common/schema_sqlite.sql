-- Biller 表结构 · 本地 SQLite 模式（幂等，可重复执行）
--
-- 与 PostgreSQL 版的差异：
--   * 自增主键用 INTEGER PRIMARY KEY AUTOINCREMENT；
--   * 没有 TIMESTAMPTZ / DATE，时间与月份统一存 **ISO 文本（UTC）**，
--     这也是 `sqlx::Any` 能同时喂两个后端的前提（Any 不支持 chrono 类型）；
--   * 布尔用 INTEGER 0/1 存；
--   * 没有 `ADD COLUMN IF NOT EXISTS`，升级列的迁移要走别的路子（见 README）。

CREATE TABLE IF NOT EXISTS bill_books (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS categories (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id INTEGER REFERENCES categories (id) ON DELETE RESTRICT,
    name      TEXT NOT NULL
);

-- 同级同名不允许。SQLite 同样支持部分索引。
CREATE UNIQUE INDEX IF NOT EXISTS categories_root_name_uniq
    ON categories (name) WHERE parent_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS categories_child_name_uniq
    ON categories (parent_id, name) WHERE parent_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS bills (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT    NOT NULL CHECK (kind IN ('income', 'expense')),
    amount      INTEGER NOT NULL CHECK (amount >= 0),
    book_id     INTEGER NOT NULL REFERENCES bill_books (id) ON DELETE RESTRICT,
    category_id INTEGER NOT NULL REFERENCES categories (id) ON DELETE RESTRICT,
    created_at  TEXT    NOT NULL,
    remark      TEXT,
    excluded    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS bills_created_at_idx  ON bills (created_at);
CREATE INDEX IF NOT EXISTS bills_book_id_idx     ON bills (book_id);
CREATE INDEX IF NOT EXISTS bills_category_id_idx ON bills (category_id);

-- 每月起始金额；month 是 `YYYY-MM-01` 文本。
CREATE TABLE IF NOT EXISTS month_balances (
    month         TEXT    PRIMARY KEY,
    start_balance INTEGER NOT NULL DEFAULT 0
);

-- 应用级键值状态（上次记账日期/账本、开销计划都在这里）。
CREATE TABLE IF NOT EXISTS app_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
