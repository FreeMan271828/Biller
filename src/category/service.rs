//! 分类业务规则：本领域对外的唯一入口，账单领域通过它解析分类。
//!
//! 分类是**树状**的（`parent_id` 自引用），因此这里还负责：
//! 把扁平列表展开成树（供终端渲染），以及在改父分类时防止成环。

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use sqlx::PgPool;

use crate::category::dao::CategoryDao;
use crate::category::model::Category;
use crate::common::db;
use crate::common::types::CategoryId;

/// 分类树展开后的一项：所在层级 + 分类本身。
#[derive(Debug, Clone)]
pub struct CategoryNode {
    pub depth: usize,
    pub category: Category,
}

/// 把扁平分类列表按父子关系展开成树：先父后子，同层按 id 升序。
///
/// 数据异常（父节点缺失、存在环）时也不丢数据：没被遍历到的分类会
/// 以顶层身份追加在末尾，避免终端里「看不见」某些分类。
pub fn flatten_tree(categories: &[Category]) -> Vec<CategoryNode> {
    let mut children: HashMap<Option<CategoryId>, Vec<&Category>> = HashMap::new();
    for category in categories {
        children.entry(category.parent_id).or_default().push(category);
    }
    for list in children.values_mut() {
        list.sort_by_key(|category| category.id);
    }

    let mut nodes = Vec::with_capacity(categories.len());
    let mut visited = HashSet::new();
    push_subtree(&children, None, 0, &mut visited, &mut nodes);

    let mut orphans: Vec<&Category> = categories
        .iter()
        .filter(|category| !visited.contains(&category.id))
        .collect();
    orphans.sort_by_key(|category| category.id);
    for orphan in orphans {
        if visited.insert(orphan.id) {
            nodes.push(CategoryNode {
                depth: 0,
                category: orphan.clone(),
            });
        }
    }

    nodes
}

fn push_subtree(
    children: &HashMap<Option<CategoryId>, Vec<&Category>>,
    parent: Option<CategoryId>,
    depth: usize,
    visited: &mut HashSet<CategoryId>,
    out: &mut Vec<CategoryNode>,
) {
    let Some(list) = children.get(&parent) else {
        return;
    };
    for category in list {
        // visited 同时承担防环职责
        if !visited.insert(category.id) {
            continue;
        }
        out.push(CategoryNode {
            depth,
            category: (*category).clone(),
        });
        push_subtree(children, Some(category.id), depth + 1, visited, out);
    }
}

/// 某分类的全部后代 id（不含自身）。
pub fn descendants_of(categories: &[Category], root: CategoryId) -> HashSet<CategoryId> {
    let mut result = HashSet::new();
    let mut stack = vec![root];

    while let Some(current) = stack.pop() {
        for category in categories
            .iter()
            .filter(|category| category.parent_id == Some(current))
        {
            if result.insert(category.id) {
                stack.push(category.id);
            }
        }
    }

    result
}

/// 某分类**自身 + 全部子孙**，按树的先后顺序（父在子前）。
pub fn subtree_of(categories: &[Category], root: CategoryId) -> Vec<Category> {
    let descendants = descendants_of(categories, root);
    flatten_tree(categories)
        .into_iter()
        .filter(|node| node.category.id == root || descendants.contains(&node.category.id))
        .map(|node| node.category)
        .collect()
}

#[derive(Clone)]
pub struct CategoryService {
    dao: CategoryDao,
}

impl CategoryService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            dao: CategoryDao::new(pool),
        }
    }

    /// 新增分类；父分类按名称解析，同级重名时报错。
    pub async fn add(&self, name: &str, parent: Option<&str>) -> Result<Category> {
        let name = normalize(name)?;
        let parent_id = match parent.map(str::trim).filter(|text| !text.is_empty()) {
            Some(parent_name) => Some(self.resolve(parent_name).await?.id),
            None => None,
        };

        if self.dao.find_sibling(parent_id, name).await?.is_some() {
            bail!("同级下已存在分类「{name}」");
        }

        let id = self
            .dao
            .insert(parent_id, name)
            .await
            .map_err(|error| db::describe(error, &format!("同级下已存在分类「{name}」")))?;

        Ok(Category {
            id,
            parent_id,
            name: name.to_string(),
        })
    }

    pub async fn list(&self) -> Result<Vec<Category>> {
        self.dao.list().await
    }

    /// 树状展开，供终端按层级缩进渲染。
    pub async fn tree(&self) -> Result<Vec<CategoryNode>> {
        Ok(flatten_tree(&self.dao.list().await?))
    }

    /// 修改名称与父分类。
    ///
    /// `new_parent` 传 `None` 表示移到顶层。会拒绝把分类挂到自己或自己的子孙下面。
    pub async fn update(
        &self,
        id: CategoryId,
        new_name: &str,
        new_parent: Option<&str>,
    ) -> Result<Category> {
        let new_name = normalize(new_name)?;

        let all = self.dao.list().await?;
        if !all.iter().any(|category| category.id == id) {
            bail!("分类 id={id} 不存在");
        }

        let parent_id = match new_parent.map(str::trim).filter(|text| !text.is_empty()) {
            Some(parent_name) => Some(self.resolve(parent_name).await?.id),
            None => None,
        };

        // 防环：不能挂到自己或自己的后代下面
        if let Some(parent_id) = parent_id {
            if parent_id == id {
                bail!("不能把分类「{new_name}」挂到它自己下面");
            }
            if descendants_of(&all, id).contains(&parent_id) {
                bail!("不能把分类「{new_name}」挂到它的子孙分类下面");
            }
        }

        if let Some(sibling) = self.dao.find_sibling(parent_id, new_name).await? {
            if sibling.id != id {
                bail!("同级下已存在分类「{new_name}」");
            }
        }

        let affected = self
            .dao
            .update(id, parent_id, new_name)
            .await
            .map_err(|error| db::describe(error, &format!("同级下已存在分类「{new_name}」")))?;
        if affected == 0 {
            bail!("分类 id={id} 不存在");
        }

        Ok(Category {
            id,
            parent_id,
            name: new_name.to_string(),
        })
    }

    /// 按名称解析；不存在或有多个同名分类时报错。
    pub async fn resolve(&self, name: &str) -> Result<Category> {
        let name = normalize(name)?;
        let matches = self.dao.list_by_name(name).await?;
        match matches.len() {
            0 => bail!("分类「{name}」不存在，可先执行：biller category add {name}"),
            1 => Ok(matches.into_iter().next().expect("长度为 1")),
            _ => {
                let ids = matches
                    .iter()
                    .map(|category| category.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("存在多个同名分类「{name}」(id: {ids})，请改用 id 指定")
            }
        }
    }

    /// 按名称解析，不存在则创建一个顶层分类；返回 (分类, 是否新建)。
    pub async fn resolve_or_create(&self, name: &str) -> Result<(Category, bool)> {
        let name = normalize(name)?;
        match self.dao.list_by_name(name).await?.as_slice() {
            [] => {
                let id = self.dao.insert(None, name).await?;
                Ok((
                    Category {
                        id,
                        parent_id: None,
                        name: name.to_string(),
                    },
                    true,
                ))
            }
            [only] => Ok((only.clone(), false)),
            many => {
                let ids = many
                    .iter()
                    .map(|category| category.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("存在多个同名分类「{name}」(id: {ids})，请改用 id 指定")
            }
        }
    }

    /// 按「id 或名称」定位，用于删除/编辑这类按目标操作的命令。
    pub async fn find_target(&self, target: &str) -> Result<Category> {
        let target = target.trim();
        if let Ok(id) = target.parse::<CategoryId>() {
            if let Some(category) = self.dao.get(id).await? {
                return Ok(category);
            }
        }
        self.resolve(target).await
    }

    /// 删除分类，返回被删掉的分类（第一项是目标自身，其余是子孙）。
    ///
    /// - 有子分类时**必须**显式传 `recursive = true`，否则报错提示；
    /// - 子树里只要还有账单就整体拒绝，避免顺手删掉账目。
    pub async fn remove(&self, target: &str, recursive: bool) -> Result<Vec<Category>> {
        let category = self.find_target(target).await?;
        let all = self.dao.list().await?;
        let subtree = subtree_of(&all, category.id);

        let child_count = subtree.len() - 1;
        if child_count > 0 && !recursive {
            bail!(
                "分类「{}」下还有 {child_count} 个子分类；确认要连同子分类一起删除请加 --recursive",
                category.name
            );
        }

        let ids: Vec<CategoryId> = subtree.iter().map(|item| item.id).collect();
        let bills = self.dao.count_bills_in(&ids).await?;
        if bills > 0 {
            bail!("这些分类下还有 {bills} 笔账单，请先删除这些账单");
        }

        // subtree 是先父后子，反过来即先从最深层删，绕开外键的 RESTRICT。
        let mut ordered = ids;
        ordered.reverse();

        self.dao
            .delete_many(&ordered)
            .await
            .map_err(|error| db::describe(error, "分类仍被引用，无法删除"))?;

        Ok(subtree)
    }

    /// 供其他领域把 id 还原成名称。
    pub async fn name_of(&self, id: CategoryId) -> Result<Option<String>> {
        Ok(self.dao.get(id).await?.map(|category| category.name))
    }
}

fn normalize(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("分类名称不能为空");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category(id: CategoryId, parent_id: Option<CategoryId>, name: &str) -> Category {
        Category {
            id,
            parent_id,
            name: name.to_string(),
        }
    }

    fn sample() -> Vec<Category> {
        // 1 餐饮 ─┬─ 2 午餐
        //         └─ 3 晚餐
        // 4 交通
        vec![
            category(4, None, "交通"),
            category(3, Some(1), "晚餐"),
            category(1, None, "餐饮"),
            category(2, Some(1), "午餐"),
        ]
    }

    #[test]
    fn flattens_tree_parent_first() {
        let nodes = flatten_tree(&sample());
        let order: Vec<(usize, &str)> = nodes
            .iter()
            .map(|node| (node.depth, node.category.name.as_str()))
            .collect();
        // 顶层按 id 升序：餐饮(1) 在前，交通(4) 在后；子节点紧跟父节点
        assert_eq!(
            order,
            vec![(0, "餐饮"), (1, "午餐"), (1, "晚餐"), (0, "交通")]
        );
    }

    #[test]
    fn keeps_orphans_visible() {
        // 5 的父节点不存在，不能丢
        let mut data = sample();
        data.push(category(5, Some(999), "孤立分类"));
        let nodes = flatten_tree(&data);
        assert_eq!(nodes.len(), data.len());
        assert!(nodes.iter().any(|node| node.category.name == "孤立分类"));
    }

    #[test]
    fn survives_cycles() {
        // 6 <-> 7 互为父节点
        let data = vec![category(6, Some(7), "甲"), category(7, Some(6), "乙")];
        let nodes = flatten_tree(&data);
        assert_eq!(nodes.len(), 2, "成环时也应全部输出且不死循环");
    }

    #[test]
    fn finds_descendants() {
        let data = sample();
        let descendants = descendants_of(&data, 1);
        assert!(descendants.contains(&2));
        assert!(descendants.contains(&3));
        assert!(!descendants.contains(&1));
        assert!(!descendants.contains(&4));
        assert!(descendants_of(&data, 4).is_empty());
    }

    #[test]
    fn collects_subtree_with_self_first() {
        let data = sample();
        let subtree = subtree_of(&data, 1);
        let names: Vec<&str> = subtree.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["餐饮", "午餐", "晚餐"]);

        // 叶子节点只有自己
        let leaf = subtree_of(&data, 2);
        assert_eq!(leaf.len(), 1);
        assert_eq!(leaf[0].name, "午餐");

        // 无关分支不受影响
        let other = subtree_of(&data, 4);
        assert_eq!(other.len(), 1);
    }
}
