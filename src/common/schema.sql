-- Biller 表结构（幂等，可重复执行）
--
-- 约定：
--   * 金额一律以「分」为单位存 BIGINT，方向由 bills.kind 决定；
--   * 时间统一存 TIMESTAMPTZ，月度统计按 UTC 归属；
--   * 月度起始金额单独存 month_balances，收入/支出/结余由 bills 实时汇总得出。

CREATE TABLE IF NOT EXISTS bill_books (
    id   BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

-- 分类支持父子层级；parent_id 为空表示顶层分类。
CREATE TABLE IF NOT EXISTS categories (
    id        BIGSERIAL PRIMARY KEY,
    parent_id BIGINT REFERENCES categories (id) ON DELETE RESTRICT,
    name      TEXT NOT NULL
);

-- 同级同名不允许。PostgreSQL 里 NULL 互不相等，所以顶层要单独建部分唯一索引。
CREATE UNIQUE INDEX IF NOT EXISTS categories_root_name_uniq
    ON categories (name) WHERE parent_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS categories_child_name_uniq
    ON categories (parent_id, name) WHERE parent_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS bills (
    id          BIGSERIAL PRIMARY KEY,
    kind        TEXT   NOT NULL CHECK (kind IN ('income', 'expense')),
    amount      BIGINT NOT NULL CHECK (amount >= 0),
    book_id     BIGINT NOT NULL REFERENCES bill_books (id) ON DELETE RESTRICT,
    category_id BIGINT NOT NULL REFERENCES categories (id) ON DELETE RESTRICT,
    created_at  TIMESTAMPTZ NOT NULL,
    remark      TEXT,
    -- 不纳入统计：报销、代付、走账这类过手钱不该算进收支与开销计划
    excluded    BOOLEAN NOT NULL DEFAULT FALSE
);

-- 老库升级：CREATE TABLE IF NOT EXISTS 不会给已存在的表补列，必须单独 ALTER。
-- 幂等，可重复执行 —— 这就是本项目「迁移 = 幂等 DDL」的用法。
ALTER TABLE bills ADD COLUMN IF NOT EXISTS excluded BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS bills_created_at_idx  ON bills (created_at);
CREATE INDEX IF NOT EXISTS bills_book_id_idx     ON bills (book_id);
CREATE INDEX IF NOT EXISTS bills_category_id_idx ON bills (category_id);

-- 每月起始金额；month 固定为该月 1 号。
CREATE TABLE IF NOT EXISTS month_balances (
    month         DATE   PRIMARY KEY,
    start_balance BIGINT NOT NULL DEFAULT 0
);

-- 应用级键值状态（目前用于记住「上次记账用的日期」）。
-- 放在数据库而不是本地文件：换机器、重启都能沿用。
CREATE TABLE IF NOT EXISTS app_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
