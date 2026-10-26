use std::collections::HashMap;

use crate::common::types::CategoryId;

/// 分类；`parent_id` 为空表示顶层分类。
#[derive(Debug, Clone)]
pub struct Category {
    pub id: CategoryId,
    pub parent_id: Option<CategoryId>,
    pub name: String,
}

/// 子分类名 → 父分类名。
///
/// 计划里按分类对账时，一笔打在「餐饮/外卖」上的账单也要算进「餐饮」的预算，
/// 所以先把父子关系摊平成一张表，再让 [`crate::plan::model::expense_by_category`]
/// 沿这张表往上加。名字相同时取先出现的那条——分类名在库里就是按父级唯一的。
pub fn parent_names(categories: &[Category]) -> HashMap<String, String> {
    let by_id: HashMap<CategoryId, &str> = categories
        .iter()
        .map(|category| (category.id, category.name.as_str()))
        .collect();

    let mut parent_of = HashMap::new();
    for category in categories {
        let Some(parent_id) = category.parent_id else {
            continue;
        };
        if let Some(parent) = by_id.get(&parent_id) {
            parent_of
                .entry(category.name.clone())
                .or_insert_with(|| (*parent).to_string());
        }
    }
    parent_of
}