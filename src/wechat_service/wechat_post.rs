use std::collections::HashSet;

use rust_decimal::Decimal;

use crate::Bill;
use crate::constant::{Company, Expense};
use crate::wechat_service::PostProcessor;

pub fn default_processors() -> Vec<PostProcessor> {
    vec![
        Box::new(identify_bikes), Box::new(identify_bikes2), 
        Box::new(identify_hik), Box::new(deduplication),
        Box::new(identify_apple),
    ]
}

pub fn deduplication(bills: Vec<Bill>) -> Vec<Bill> {
    let set:HashSet<Bill> = bills.into_iter().collect();
    set.into_iter().collect()
}

/// 用于识别Apple
pub fn identify_apple(bills: Vec<Bill>) -> Vec<Bill>{
    let mut bills = bills;
    for bill in &mut bills{
        let lower = bill.company.to_lowercase();
        let count = lower.chars()
            .filter(|&c| c == 'a' || c == 'p' || c == 'l' || c == 'e')
            .count();
        if count >= 4 {
            bill.company = Company::Apple.as_str().to_string();
        }    
    }
    bills
}

/// 用于识别海康的外部账单
pub fn identify_hik(bills: Vec<Bill>) -> Vec<Bill> {
    let mut bills = bills;
    let hik_company = "杭州海康威视";
    for bill in &mut bills {
        if bill.company.contains(hik_company) {
            bill.company = Company::HikOut.as_str().to_string();
        }
    }
    bills
}

/// 限额max_money+识别公司
pub fn identify_bikes(bills: Vec<Bill>) -> Vec<Bill> {
    let max_money = Decimal::new(2, 2);
    let mut bills = bills;
    let bike_companies = vec![Company::MeiTuan.as_str()];
    for bill in &mut bills {
        let is_bike_company = bike_companies
            .iter()
            .any(|&company| bill.company.contains(company));
        if bill.number.abs() <= max_money  && is_bike_company && Expense::Transportation.as_str().ne(&bill.kind){
            bill.kind = Expense::Transportation.as_str().to_string();
        }
    }
    bills
}

/// 识别公司
pub fn identify_bikes2(bills: Vec<Bill>) -> Vec<Bill> {
    let mut bills = bills;
    let bike_companies = vec![Company::BaiXingKuaiZu.as_str(), Company::HelloTraffic.as_str()];
    for bill in &mut bills {
        let is_bike_company = bike_companies
            .iter()
            .any(|&company| bill.company.contains(company));
        if is_bike_company && Expense::Transportation.as_str().ne(&bill.kind){
            bill.kind = Expense::Transportation.as_str().to_string();
        }
    }
    bills
}