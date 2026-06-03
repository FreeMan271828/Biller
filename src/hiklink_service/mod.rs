use chrono::{Datelike, Local};
use regex::Regex;
use rust_decimal::Decimal;

use crate::{Bill, BillService, PostProcess, PostProcessor, PreProcess, PreProcessor, constant::{Company, Expense, RegexRule}, database::Repository};

pub struct HikLinkService{
    pre_processors: Vec<PreProcessor>,
    post_processors: Vec<PostProcessor>,
}

impl HikLinkService{
    pub fn new() -> Self{
        HikLinkService { pre_processors: Vec::new(), post_processors: Vec::new() }
    }

    fn extract_money_index(input: &Vec<String>) -> Vec<usize> {
        input.iter().enumerate()
            .filter(|(_, value)| RegexRule::NumberRule.as_regex().is_match(value))
            .map(|(index, _)| index)
            .collect()
    }

    fn find_date_in_range(input: &[String], start_idx: usize, end_idx: usize, current_year: i32, time_regex: &Regex) -> Option<String> {
        for idx in start_idx..end_idx {
            if let Some(text) = input.get(idx) {
                if let Some(caps) = time_regex.captures(text) {
                    return Some(format!("{}-{}-{}", current_year, &caps[1], &caps[2]));
                }
            }
        }
        None
    }

    fn extract_target_date(input: &[String], current_year: i32, time_regex: &Regex) -> String {
        input
            .iter()
            .find_map(|s| {
                time_regex.captures(s).map(|caps| {
                    format!("{}-{}-{}", current_year, &caps[1], &caps[2])
                })
            })
            .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string())
    }

    fn build_bill(input: &[String], money_idx: usize, target_date: &str) -> Bill {
        let mut bill: Bill = Default::default();
        bill.number = input
            .get(money_idx)
            .unwrap()
            .parse::<Decimal>()
            .unwrap_or(Decimal::default());
        bill.company = Company::HikIn.as_str().to_string();
        bill.kind = Expense::Catering.as_str().to_string();
        bill.time = target_date.to_string();
        bill
    }

}

#[async_trait::async_trait]
impl BillService for HikLinkService {
    fn parse(&self, input: Vec<String>) -> Vec<Bill> {
        let mut processed_input = input;
        for pre_proc in &self.pre_processors {
            processed_input = pre_proc(processed_input);
        }
        
        let marked = Self::extract_money_index(&processed_input);
        let current_year = Local::now().year();
        let time_regex = RegexRule::HikLinkTimeRule.as_regex();

        let target_date = Self::extract_target_date(&processed_input, current_year, &time_regex);
        let mut bills: Vec<Bill> = vec![];
        for pair in marked.windows(2) {
            let curr_money_idx = pair[0];
            let next_money_idx = pair[1];

            if let Some(bill_date) = Self::find_date_in_range(&processed_input, curr_money_idx + 1, next_money_idx, current_year, &time_regex) {
                if bill_date != target_date {
                    break;
                }
            }
            bills.push(Self::build_bill(&processed_input, curr_money_idx, &target_date));
        }
        // 4. 独立处理最后一个金额（区间从最后一个金额到数组末尾）
        if let Some(&last_money_idx) = marked.last() {
            let end_idx = processed_input.len();
            let mut should_add_last = true;

            if let Some(bill_date) = Self::find_date_in_range(&processed_input, last_money_idx + 1, end_idx, current_year, &time_regex) {
                if bill_date != target_date {
                    should_add_last = false; // 最后一笔跨天，不添加
                }
            }
            if should_add_last {
                bills.push(Self::build_bill(&processed_input, last_money_idx, &target_date));
            }
        }

        for post_proc in &self.post_processors {
            bills = post_proc(bills);
        }
        bills
    }


    async fn storage(bills: Vec<Bill>, pool: &(dyn Repository<Bill> + Send + Sync)){
        pool.inserts(&bills).await.expect("Something Err in Insert");
    }
}

impl PreProcess for HikLinkService{
    fn add_processor(mut self, pre_processor: PreProcessor) -> Self {
        self.pre_processors.push(pre_processor);
        self
    }
}

impl PostProcess for HikLinkService{
    fn add_processor(mut self, post_processor: PostProcessor) -> Self {
        self.post_processors.push(post_processor);
        self
    }
}