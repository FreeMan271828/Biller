//! 账本的终端交互层：只做参数解析与输出格式化。

use anyhow::Result;
use clap::Subcommand;

use crate::bill_book::service::BookService;
use crate::common::table;

#[derive(Subcommand, Debug)]
pub enum BookCmd {
    /// 列出全部账本
    List,
    /// 新增账本
    Add {
        /// 账本名称
        name: String,
    },
    /// 重命名账本（按 id 或名称）
    Edit {
        /// 账本 id 或名称
        target: String,
        /// 新名称
        #[arg(long)]
        name: String,
    },
    /// 删除账本（按 id 或名称）
    Rm {
        /// 账本 id 或名称
        target: String,
    },
}

pub async fn run(service: &BookService, command: BookCmd) -> Result<()> {
    match command {
        BookCmd::List => {
            let books = service.list().await?;
            let rows = books
                .iter()
                .map(|book| vec![book.id.to_string(), book.name.clone()])
                .collect::<Vec<_>>();
            table::print_table(&["ID", "账本"], &rows);
        }
        BookCmd::Add { name } => {
            let book = service.add(&name).await?;
            println!("已新增账本「{}」(id={})", book.name, book.id);
        }
        BookCmd::Edit { target, name } => {
            let current = service.find_target(&target).await?;
            let updated = service.rename(current.id, &name).await?;
            println!(
                "账本「{}」(id={}) 已重命名为「{}」",
                current.name, updated.id, updated.name
            );
        }
        BookCmd::Rm { target } => {
            let book = service.remove(&target).await?;
            println!("已删除账本「{}」(id={})", book.name, book.id);
        }
    }
    Ok(())
}
