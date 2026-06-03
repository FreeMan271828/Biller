use regex::Regex;

use crate::{constant::RegexRule, wechat_service::PreProcessor};

pub fn default_processors() -> Vec<PreProcessor> {
    vec![
        Box::new(clean_data), Box::new(filter_by_date_range)
    ]
}

pub fn filter_by_date_range(input: Vec<String>) -> Vec<String> {
    let mut first_date_idx = None;
    let mut second_date_idx = None;

    for (index, item) in input.iter().enumerate() {
        if RegexRule::WechatDateRule.as_regex().is_match(item.trim()) {
            if first_date_idx.is_none() {
                first_date_idx = Some(index);
            } else {
                second_date_idx = Some(index);
                break;
            }
        }
    }
    match (first_date_idx, second_date_idx) {
        (Some(start), Some(end)) => {
            input[start..end].to_vec()
        }
        (Some(start), None) => {
            input[start..].to_vec()
        }
        _ => {
            input
        }
    }
}


/// 清理数据
pub fn clean_data(input: Vec<String>) -> Vec<String> {
    let gexes = vec![
        Regex::new(r"^[a-zA-Z]$").unwrap(),
        Regex::new(r"^\d{1,4}年\d{1,2}月").unwrap(),
        Regex::new(r"^[出入]\d+(\.\d+)?$").unwrap(),
        Regex::new(r"记一笔").unwrap(),
        Regex::new(r"总支出").unwrap(), Regex::new(r"总入账").unwrap(),
    ];
    input
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| {
            for gex in &gexes{
                if gex.is_match(item){
                    return false;
                }
            }
            true
        })
        .collect()
}
