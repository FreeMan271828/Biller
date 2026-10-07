use crate::common::types::CategoryId;

/// 分类；`parent_id` 为空表示顶层分类。
#[derive(Debug, Clone)]
pub struct Category {
    pub id: CategoryId,
    pub parent_id: Option<CategoryId>,
    pub name: String,
}
