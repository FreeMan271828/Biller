use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RegexRule{
    NumberRule, HikLinkTimeRule, WechatTimeAddressRule, WechatDateRule,
}
#[allow(dead_code)]
impl RegexRule {
    pub fn as_regex(&self) -> Regex {
        match self {
           Self::NumberRule => Regex::new(r"^[+-]\d+(?:\.\d+)?$").unwrap(),
           Self::HikLinkTimeRule => Regex::new(r"^(\d{2})-(\d{2})\s*(\d{2}):(\d{2})").unwrap(),
           Self::WechatTimeAddressRule => Regex::new(r"^(?P<time>\d{1,2}:\d{2})[| \-]*(?P<location>.+)$").unwrap(),
           Self::WechatDateRule => Regex::new(r"^\d{1,2}月\d{1,2}日").unwrap(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Company{
    MeiTuan, HikOut, HikIn, Apple, BaiXingKuaiZu, HelloTraffic
}
impl Company {
    pub fn as_str(&self) -> &'static str {
        match self {
            Company::MeiTuan => "美团", Company::HikOut => "海康外部", Company::HikIn => "海康内部", Company::Apple => "苹果", 
            Company::BaiXingKuaiZu => "百姓快租", Company::HelloTraffic => "哈啰出行", 
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Expense { // === 支出类 ===
    Catering, Transportation, Apparel, Shopping, Service, Education, Entertainment, Sports,
    UtilityBills, Travel, Pets, Medical, Insurance, Charity, RedPacket, Transfer, RelativeCard,
    Interpersonal, Other, Refund,
}
#[allow(dead_code)]
impl Expense {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Catering => "餐饮", Self::Transportation => "交通", Self::Apparel => "服饰", Self::Shopping => "购物",
            Self::Service => "服务", Self::Education => "教育", Self::Entertainment => "娱乐", Self::Sports => "运动",
            Self::UtilityBills => "生活缴费", Self::Travel => "旅行", Self::Pets => "宠物", Self::Medical => "医疗",
            Self::Insurance => "保险", Self::Charity => "公益", Self::RedPacket => "发红包", Self::Transfer => "转账",
            Self::RelativeCard => "亲属卡", Self::Interpersonal => "其他人情", Self::Other => "其他", Self::Refund => "退还",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Income { // === 收入类 ===
    Business, Salary, Bonus, Interpersonal, ReceiveRedPacket, ReceiveTransfer, MerchantTransfer,
    Refund, Other,
}
#[allow(dead_code)]
impl Income {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Business => "生意", Self::Salary => "工资", Self::Bonus => "奖金", Self::Interpersonal => "其他人情",
            Self::ReceiveRedPacket => "收红包", Self::ReceiveTransfer => "收转账", Self::MerchantTransfer => "商家转账",
            Self::Refund => "退款", Self::Other => "其他",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Excluded { // === 不计入收支类 ===
    Financial, LoanAndRepayment, Other,
}
#[allow(dead_code)]
impl Excluded {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Financial => "理财", Self::LoanAndRepayment => "借还款", Self::Other => "其他",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Record { // === 统一包装大类 ===
    Expense(Expense),
    Income(Income),
    Excluded(Excluded),
}
#[allow(dead_code)]
impl Record {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expense(e) => e.as_str(),
            Self::Income(i) => i.as_str(),
            Self::Excluded(ex) => ex.as_str(),
        }
    }
}
