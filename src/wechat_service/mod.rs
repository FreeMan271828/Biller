mod wechat_pre;
mod wechat_post;

use crate::{PostProcess, PostProcessor, PreProcess, PreProcessor, constant::RegexRule, database::Repository};
use chrono::{Datelike, Local};
use rust_decimal::Decimal;
pub use crate::{Bill, BillService};


pub struct WechatService{
    pre_processors: Vec<PreProcessor>,
    post_processors: Vec<PostProcessor>,
}
impl WechatService {
    pub fn new() -> Self {
        Self {
            pre_processors: wechat_pre::default_processors(),
            post_processors: wechat_post::default_processors(),
        }
    }

    fn extract_money_index(input: &Vec<String>) -> Vec<usize> {
        input.iter().enumerate()
            .filter(|(_, value)| RegexRule::NumberRule.as_regex().is_match(value))
            .map(|(index, _)| index)
            .collect()
    }
    
    fn core_parse(input: &Vec<String>, marked: Vec<usize>) -> Vec<Bill> {
        let mut bills: Vec<Bill> = Default::default();
        let current_year = Local::now().year();
        let num_extractor = regex::Regex::new(r"\d+").unwrap();
        for index in marked {
            let mut bill: Bill = Default::default();
            bill.number = input.get(index).unwrap().parse::<Decimal>().unwrap_or(Decimal::default());
            if index > 0 {
                if let Some(prev_val) = input.get(index - 1) {
                    bill.kind = prev_val.to_string();
                }
            }
            let mut base_date = String::new();
            for i in (0..index).rev() {
                if let Some(line) = input.get(i) {
                    if RegexRule::WechatDateRule.as_regex().is_match(line) {
                        let nums: Vec<&str> = num_extractor.find_iter(line).map(|m| m.as_str()).collect();
                        if nums.len() >= 2 {
                            let month: i32 = nums[0].parse().unwrap_or(1);
                            let day: i32 = nums[1].parse().unwrap_or(1);
                            base_date = format!("{}-{:02}-{:02}", current_year, month, day);
                            break; 
                        }
                    }
                }
            }
            if let Some(val) = input.get(index + 1) {
                if let Some(caps) = RegexRule::WechatTimeAddressRule.as_regex().captures(val) {
                    let location = &caps["location"];
                    let time = &caps["time"];
                    bill.company = location.to_string();
                    bill.remark = time.to_string();
                } else if let Some(val2) = input.get(index + 2) {
                    if let Some(caps) = RegexRule::WechatTimeAddressRule.as_regex().captures(val2) {
                        let location = &caps["location"];
                        let time = &caps["time"];
                        bill.company = location.to_string();
                        bill.remark = time.to_string();
                    } else {
                        println!("商户提取失败");
                    }
                }
            }
            if !base_date.is_empty() {
                bill.time = base_date; 
            }
            else {
                bill.time = Local::now().format("%Y-%m-%d").to_string();
            }

            bills.push(bill);
        }
        bills
    }
}

#[async_trait::async_trait]
impl BillService for WechatService {
    fn parse(&self, input: Vec<String>) -> Vec<Bill>{
        let mut processed_input = input;
        for pre_proc in &self.pre_processors {
            processed_input = pre_proc(processed_input);
        }
        let marked = Self::extract_money_index(&processed_input);
        let mut bills = Self::core_parse(&processed_input, marked);
        for post_proc in &self.post_processors {
            bills = post_proc(bills);
        }
        bills
    }

    // 存储进数据库
    async fn storage(bills: Vec<Bill>, pool: &(dyn Repository<Bill> + Send + Sync)){
        pool.inserts(&bills).await.expect("Something Err in Insert");
    }
}

impl PreProcess for WechatService {
    fn add_processor(mut self, pre_processor: PreProcessor) -> Self{
        self.pre_processors.push(Box::new(pre_processor));
        self
    }
}

impl PostProcess for WechatService {
    fn add_processor(mut self, post_processor: PostProcessor) -> Self{
        self.post_processors.push(Box::new(post_processor));
        self
    }
}


#[cfg(test)]
mod tests {
}