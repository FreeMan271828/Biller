mod bill_repository;
pub use bill_repository::BillRepository;

#[async_trait::async_trait]
pub trait Repository<T>{
    async fn insert(&self, data: &T) -> Result<(), sqlx::Error>;
    async fn inserts(&self, data: &Vec<T>) -> Result<(), sqlx::Error>;
}