use std::env;
use dotenvy::dotenv;
use sqlx::{Error, PgPool};
use crate::Bill;
use crate::database::Repository;

pub struct BillRepository {
    pool: PgPool,
}
impl BillRepository {
    // 构造函数应该属于特殊结构体独有的函数，而不是作为公有的
    pub async fn new() -> Result<Self, Error> {
        dotenv().ok();
        let url = env::var("DATABASE_URL").expect("必须在环境变量或 .env 中设置 DATABASE_URL");
        let pool = PgPool::connect(&url).await?;
        Ok(BillRepository { pool })
    }
}

#[async_trait::async_trait]
impl Repository<Bill> for BillRepository {
    async fn insert(&self, bill: &Bill) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO bills (number, kind, time, company) VALUES ($1, $2, $3, $4)"
        )
            .bind(bill.number)
            .bind(&bill.kind)
            .bind(&bill.time)
            .bind(&bill.company)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn inserts(&self, bills: &Vec<Bill>) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        for bill in bills {
            sqlx::query("INSERT INTO bills (number, kind, time, company) VALUES ($1, $2, $3, $4)")
                .bind(bill.number)
                .bind(&bill.kind)
                .bind(&bill.time)
                .bind(&bill.company)
                .execute(&mut *tx) // 注意：在事务中执行需要传入 &mut *tx
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}