use std::{error::Error, path::Path};

use futures::future::BoxFuture;
use rust_decimal::Decimal;

use crate::{database::{BillRepository, Repository}, hiklink_service::HikLinkService, ocr::parse_image, wechat_service::WechatService};

pub mod ocr;
mod constant;
pub mod wechat_service;
pub mod hiklink_service;
pub mod database;

pub type PreProcessor = Box<dyn Fn(Vec<String>) -> Vec<String> + Send + Sync>;
pub type PostProcessor= Box<dyn Fn(Vec<Bill>) -> Vec<Bill> + Send + Sync>;
pub type MainFunction = Box<dyn for<'a> Fn(&'a BillRepository) -> BoxFuture<'a, Result<(), Box<dyn Error>>>>;

#[allow(unused)]
pub async fn hiklink_main(repo: &BillRepository)-> Result<(), Box<dyn Error>>{
    let service = HikLinkService::new();

    let path = Path::new("pictures/hiklink");
    let mut data:Vec<String> = Vec::new();
    for file in path.read_dir()? {
        let file = file?.path();
        if let Some(path_str) = file.to_str() {
            data.extend(parse_image(path_str.as_ref())?);
        }
    }
    println!("{:?}",data);
    let data= service.parse(data);
    println!("{:?}",data);
    HikLinkService::storage(data, repo).await;
    Ok(())
}

#[allow(unused)]
pub async fn wechat_main(repo: &BillRepository)-> Result<(), Box<dyn Error>>{
    let service = WechatService::new();

    let path = Path::new("pictures/wechat");
    let mut data:Vec<String> = Vec::new();
    for file in path.read_dir()? {
        let file = file?.path();
        if let Some(path_str) = file.to_str() {
            data.extend(parse_image(path_str.as_ref())?);
        }
    }
    println!("{:?}", data);
    let data= service.parse(data);
    println!("{:?}", data);
    WechatService::storage(data, repo).await;
    Ok(())
}

pub trait PreProcess: Send + Sync {
    fn add_processor(self, pre_processor: PreProcessor) -> Self;
}

pub trait PostProcess: Send + Sync {
    fn add_processor(self, post_processor: PostProcessor) -> Self;
}

#[async_trait::async_trait]
pub trait BillService {
    fn parse(&self, input: Vec<String>) ->Vec<Bill>;
    async fn storage(bills: Vec<Bill>, pool: &(dyn Repository<Bill> + Send + Sync));
}

#[derive(Default, Debug, PartialEq, Eq, Hash)]
pub struct Bill{
    pub number: Decimal,
    pub kind: String,
    pub time: String,
    pub company: String,
    pub remark: String,
}
impl Bill {
    pub fn new(number: Decimal, kind: String, time: String, location: String) -> Self {
        Self {
            number,
            kind,
            time,
            company: location,
            remark: String::new(),
        }
    }
}