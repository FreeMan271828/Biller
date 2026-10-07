//! 分类的终端交互层：只做参数解析与输出格式化。

use std::collections::HashMap;

use anyhow::Result;
use clap::Subcommand;

use crate::category::service::{CategoryNode, CategoryService};
use crate::common::table;
use crate::common::types::CategoryId;

#[derive(Subcommand, Debug)]
pub enum CategoryCmd {
    /// 按层级列出全部分类
    List,
    /// 新增分类
    Add {
        /// 分类名称
        name: String,
        /// 父分类名称
        #[arg(long)]
        parent: Option<String>,
    },
    /// 修改分类：改名或调整父分类
    Edit {
        /// 分类 id 或名称
        target: String,
        /// 新名称；省略则保持不变
        #[arg(long)]
        name: Option<String>,
        /// 新的父分类名称；省略则保持不变
        #[arg(long)]
        parent: Option<String>,
        /// 移动到顶层（清空父分类）
        #[arg(long, conflicts_with = "parent")]
        root: bool,
    },
    /// 删除分类（按 id 或名称）
    Rm {
        /// 分类 id 或名称
        target: String,
        /// 连同所有子孙分类一起删除
        #[arg(long, short)]
        recursive: bool,
    },
}

pub async fn run(service: &CategoryService, command: CategoryCmd) -> Result<()> {
    match command {
        CategoryCmd::List => {
            let nodes = service.tree().await?;
            let rows = nodes
                .iter()
                .map(|node| {
                    let indent = if node.depth == 0 {
                        String::new()
                    } else {
                        format!("{}└─ ", "  ".repeat(node.depth - 1))
                    };
                    let parent = match node.category.parent_id {
                        Some(id) => node_name(&nodes, id),
                        None => "-".to_string(),
                    };
                    vec![
                        node.category.id.to_string(),
                        format!("{indent}{}", node.category.name),
                        parent,
                    ]
                })
                .collect::<Vec<_>>();

            table::print_table(&["ID", "分类", "父分类"], &rows);
        }
        CategoryCmd::Add { name, parent } => {
            let category = service.add(&name, parent.as_deref()).await?;
            match category.parent_id {
                Some(parent_id) => println!(
                    "已新增分类「{}」(id={}, 父分类 id={})",
                    category.name, category.id, parent_id
                ),
                None => println!("已新增分类「{}」(id={})", category.name, category.id),
            }
        }
        CategoryCmd::Edit {
            target,
            name,
            parent,
            root,
        } => {
            let current = service.find_target(&target).await?;
            let new_name = name.unwrap_or_else(|| current.name.clone());

            // 未指定 --parent / --root 时保持原父分类
            let new_parent: Option<String> = if root {
                None
            } else if parent.is_some() {
                parent
            } else {
                let names: HashMap<CategoryId, String> = service
                    .list()
                    .await?
                    .into_iter()
                    .map(|category| (category.id, category.name))
                    .collect();
                current.parent_id.and_then(|id| names.get(&id).cloned())
            };

            let updated = service
                .update(current.id, &new_name, new_parent.as_deref())
                .await?;

            let parent_text = match updated.parent_id {
                Some(id) => format!("父分类 id={id}"),
                None => "顶层".to_string(),
            };
            println!(
                "分类「{}」(id={}) 已更新为「{}」（{parent_text}）",
                current.name, updated.id, updated.name
            );
        }
        CategoryCmd::Rm { target, recursive } => {
            let removed = service.remove(&target, recursive).await?;
            let (root, children) = removed.split_first().expect("至少包含目标自身");

            if children.is_empty() {
                println!("已删除分类「{}」(id={})", root.name, root.id);
            } else {
                let names = children
                    .iter()
                    .map(|item| item.name.clone())
                    .collect::<Vec<_>>()
                    .join("、");
                println!(
                    "已删除分类「{}」(id={}) 及其 {} 个子分类：{}",
                    root.name,
                    root.id,
                    children.len(),
                    names
                );
            }
        }
    }
    Ok(())
}

fn node_name(nodes: &[CategoryNode], id: CategoryId) -> String {
    nodes
        .iter()
        .find(|node| node.category.id == id)
        .map(|node| node.category.name.clone())
        .unwrap_or_else(|| format!("id={id}"))
}
