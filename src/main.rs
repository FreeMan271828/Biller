use biller::database::{BillRepository};
use biller::{MainFunction, hiklink_main, wechat_main};

macro_rules! task {
    ($func:expr) => {
        Box::new(|repo| ::futures::FutureExt::boxed(async move { $func(repo).await }))
    };
}

pub fn main_functions() -> Vec<MainFunction>{
    vec![
        task!(wechat_main),
        task!(hiklink_main),
    ]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>>{
    let repo = BillRepository::new().await?;
    for function in main_functions(){
        if let Err(e) = function(&repo).await{
            println!("{:?}", e);
            break;
        }
    }
    Ok(())
}
