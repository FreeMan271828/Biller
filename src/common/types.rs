//! 跨模块共享的类型别名。

/// 账单主键。
pub type BillId = u64;
/// 账本主键。
pub type BookId = u64;
/// 分类主键。
pub type CategoryId = u64;

/// 金额：以「分」为单位的整数，避免浮点误差。
pub type AmountType = i64;
