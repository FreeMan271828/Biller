//! 账本业务规则：本领域对外的唯一入口，账单领域通过它解析账本。

use anyhow::{Context, Result, bail};
use sqlx::PgPool;

use crate::bill_book::dao::BookDao;
use crate::bill_book::model::BillBook;
use crate::common::db;
use crate::common::types::BookId;

#[derive(Clone)]
pub struct BookService {
    dao: BookDao,
}

impl BookService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            dao: BookDao::new(pool),
        }
    }

    /// 新增账本；重名时报错。
    pub async fn add(&self, name: &str) -> Result<BillBook> {
        let name = normalize(name)?;
        if self.dao.find_by_name(name).await?.is_some() {
            bail!("账本「{name}」已存在");
        }
        let id = self
            .dao
            .insert(name)
            .await
            .map_err(|error| db::describe(error, &format!("账本「{name}」已存在")))?;
        Ok(BillBook {
            id,
            name: name.to_string(),
        })
    }

    pub async fn list(&self) -> Result<Vec<BillBook>> {
        self.dao.list().await
    }

    /// 按名称查找，不存在则报错。
    pub async fn resolve(&self, name: &str) -> Result<BillBook> {
        let name = normalize(name)?;
        self.dao.find_by_name(name).await?.with_context(|| {
            format!("账本「{name}」不存在，可先执行：biller book add {name}")
        })
    }

    /// 按名称查找，不存在则自动创建；返回 (账本, 是否新建)。
    pub async fn resolve_or_create(&self, name: &str) -> Result<(BillBook, bool)> {
        let name = normalize(name)?;
        match self.dao.find_by_name(name).await? {
            Some(book) => Ok((book, false)),
            None => {
                let id = self.dao.insert(name).await?;
                Ok((
                    BillBook {
                        id,
                        name: name.to_string(),
                    },
                    true,
                ))
            }
        }
    }

    /// 按「id 或名称」定位，用于删除这类按目标操作的命令。
    pub async fn find_target(&self, target: &str) -> Result<BillBook> {
        let target = target.trim();
        if let Ok(id) = target.parse::<BookId>() {
            if let Some(book) = self.dao.get(id).await? {
                return Ok(book);
            }
        }
        self.resolve(target).await
    }

    /// 改名。
    pub async fn rename(&self, id: BookId, new_name: &str) -> Result<BillBook> {
        let new_name = normalize(new_name)?;

        let current = self
            .dao
            .get(id)
            .await?
            .with_context(|| format!("账本 id={id} 不存在"))?;

        if current.name == new_name {
            return Ok(current);
        }

        if let Some(existing) = self.dao.find_by_name(new_name).await? {
            if existing.id != id {
                bail!("账本「{new_name}」已存在");
            }
        }

        let affected = self
            .dao
            .update(id, new_name)
            .await
            .map_err(|error| db::describe(error, &format!("账本「{new_name}」已存在")))?;
        if affected == 0 {
            bail!("账本 id={id} 不存在");
        }

        Ok(BillBook {
            id,
            name: new_name.to_string(),
        })
    }

    /// 删除账本；仍被账单引用时给出可读提示。
    pub async fn remove(&self, target: &str) -> Result<BillBook> {
        let book = self.find_target(target).await?;

        // 先显式预检引用关系，比依赖外键报错更友好、也更稳定。
        let bills = self.dao.count_bills(book.id).await?;
        if bills > 0 {
            bail!(
                "账本「{}」下还有 {bills} 笔账单，请先删除这些账单",
                book.name
            );
        }

        let affected = self.dao.delete(book.id).await.map_err(|error| {
            db::describe(error, &format!("账本「{}」仍被引用，无法删除", book.name))
        })?;
        if affected == 0 {
            bail!("账本「{}」不存在", book.name);
        }
        Ok(book)
    }

    /// 供其他领域把 id 还原成名称。
    pub async fn name_of(&self, id: BookId) -> Result<Option<String>> {
        Ok(self.dao.get(id).await?.map(|book| book.name))
    }
}

fn normalize(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("账本名称不能为空");
    }
    Ok(name)
}
