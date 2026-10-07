use crate::common::types::BookId;

/// 账本。
#[derive(Debug, Clone)]
pub struct BillBook {
    pub id: BookId,
    pub name: String,
}
