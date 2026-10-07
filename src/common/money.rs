//! 金额工具。
//!
//! 数据模型里 `AmountType = i64`，约定这个整数就是**分**：
//! 命令行输入 `12.34`（元）会被解析成 `1234`（分）落库，
//! 反过来查询输出时再格式化成 `12.34`。

use anyhow::{Result, anyhow, bail};

use crate::common::types::AmountType;

/// 把用户输入的「元」金额解析成「分」。
///
/// 支持 `12`、`12.3`、`12.34`、`1,234.56`、`+12.34`；
/// 拒绝负数（收入/支出方向请用 `kind` 参数表达），
/// 拒绝超过两位的小数，避免静默丢精度。
pub fn parse_amount(input: &str) -> Result<AmountType> {
    let cleaned = input.trim().replace(',', "").replace('_', "");
    let body = cleaned
        .trim()
        .strip_prefix('+')
        .unwrap_or(cleaned.trim())
        .trim();

    if body.is_empty() {
        bail!("金额不能为空");
    }
    if body.starts_with('-') {
        bail!("金额不能为负；收入/支出请用 kind 参数区分（income / expense）");
    }

    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        bail!("金额格式不正确：{input}");
    }
    if !int_part.chars().all(|c| c.is_ascii_digit()) {
        bail!("金额的整数部分只能是数字：{input}");
    }
    if !frac_part.chars().all(|c| c.is_ascii_digit()) {
        bail!("金额的小数部分只能是数字：{input}");
    }
    if frac_part.len() > 2 {
        bail!("金额最多两位小数（精确到分），收到：{input}");
    }

    let yuan: AmountType = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| anyhow!("金额整数部分超出范围：{input}"))?
    };

    let mut cents_str = frac_part.to_string();
    while cents_str.len() < 2 {
        cents_str.push('0');
    }
    let cents: AmountType = cents_str.parse().expect("已校验为纯数字");

    yuan
        .checked_mul(100)
        .and_then(|value| value.checked_add(cents))
        .ok_or_else(|| anyhow!("金额超出可表示范围：{input}"))
}

/// 把「分」格式化成「元」，例如 `1234 -> "12.34"`、`-5 -> "-0.05"`。
pub fn format_amount(cents: AmountType) -> String {
    let value = cents as i128;
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// 计算简单算式，返回按「元」格式化的结果（两位小数）。
///
/// 支持 `+ - * /`、括号与一元正负号，例如 `12.34+5`、`(1+2)*3`、`-4/2`；
/// 全角符号（`＋－×÷（）`）、`x` 乘号、千分位逗号与空白都会自动规整。
///
/// 中间用 `f64` 计算，最后按「分」四舍五入，因此 `0.1+0.2` 不会留下浮点尾巴。
pub fn evaluate_expression(input: &str) -> Result<String> {
    let normalized = normalize_expression(input);
    if normalized.is_empty() {
        bail!("金额不能为空");
    }

    let mut parser = ExpressionParser::new(&normalized);
    let value = parser.parse_expression()?;
    if !parser.at_end() {
        bail!("算式里有无法识别的内容：{}", input.trim());
    }
    if !value.is_finite() {
        bail!("算式结果不是有效数字：{}", input.trim());
    }

    let cents = (value * 100.0).round();
    if cents.abs() >= i64::MAX as f64 {
        bail!("算式结果超出可表示范围：{}", input.trim());
    }
    Ok(format_amount(cents as i64))
}

/// 把算式规整成只含 ASCII 运算符与数字的形式。
fn normalize_expression(input: &str) -> String {
    input
        .chars()
        .filter_map(|character| match character {
            '＋' => Some('+'),
            '－' | '−' | '—' => Some('-'),
            '＊' | '×' | 'x' | 'X' => Some('*'),
            '／' | '÷' => Some('/'),
            '（' => Some('('),
            '）' => Some(')'),
            '。' => Some('.'),
            // 千分位与空白直接丢掉
            ',' | '，' | '_' | ' ' | '\t' => None,
            other => Some(other),
        })
        .collect()
}

/// 递归下降解析：expression → term → factor，优先级由层级体现。
struct ExpressionParser {
    characters: Vec<char>,
    position: usize,
}

impl ExpressionParser {
    fn new(input: &str) -> Self {
        Self {
            characters: input.chars().collect(),
            position: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.characters.get(self.position).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek();
        if character.is_some() {
            self.position += 1;
        }
        character
    }

    fn at_end(&self) -> bool {
        self.position >= self.characters.len()
    }

    fn parse_expression(&mut self) -> Result<f64> {
        let mut value = self.parse_term()?;
        loop {
            match self.peek() {
                Some('+') => {
                    self.bump();
                    value += self.parse_term()?;
                }
                Some('-') => {
                    self.bump();
                    value -= self.parse_term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_term(&mut self) -> Result<f64> {
        let mut value = self.parse_factor()?;
        loop {
            match self.peek() {
                Some('*') => {
                    self.bump();
                    value *= self.parse_factor()?;
                }
                Some('/') => {
                    self.bump();
                    let divisor = self.parse_factor()?;
                    if divisor == 0.0 {
                        bail!("算式里出现了除以 0");
                    }
                    value /= divisor;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_factor(&mut self) -> Result<f64> {
        match self.peek() {
            Some('+') => {
                self.bump();
                self.parse_factor()
            }
            Some('-') => {
                self.bump();
                Ok(-self.parse_factor()?)
            }
            Some('(') => {
                self.bump();
                let value = self.parse_expression()?;
                if self.bump() != Some(')') {
                    bail!("算式括号不匹配");
                }
                Ok(value)
            }
            Some(character) if character.is_ascii_digit() || character == '.' => self.parse_number(),
            _ => bail!("算式不完整或含非法字符"),
        }
    }

    fn parse_number(&mut self) -> Result<f64> {
        let start = self.position;
        let mut seen_dot = false;

        while let Some(character) = self.peek() {
            if character.is_ascii_digit() {
                self.position += 1;
            } else if character == '.' && !seen_dot {
                seen_dot = true;
                self.position += 1;
            } else {
                break;
            }
        }

        let text: String = self.characters[start..self.position].iter().collect();
        text.parse::<f64>()
            .map_err(|_| anyhow!("无法解析数字：{text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yuan_and_cents() {
        assert_eq!(parse_amount("12").unwrap(), 1200);
        assert_eq!(parse_amount("12.3").unwrap(), 1230);
        assert_eq!(parse_amount("12.34").unwrap(), 1234);
        assert_eq!(parse_amount("0.05").unwrap(), 5);
        assert_eq!(parse_amount(".5").unwrap(), 50);
        assert_eq!(parse_amount("+8.00").unwrap(), 800);
        assert_eq!(parse_amount(" 1,234.56 ").unwrap(), 123456);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_amount("").is_err());
        assert!(parse_amount("-1").is_err());
        assert!(parse_amount("1.234").is_err());
        assert!(parse_amount("abc").is_err());
        assert!(parse_amount("1.2.3").is_err());
    }

    #[test]
    fn formats_back() {
        assert_eq!(format_amount(1234), "12.34");
        assert_eq!(format_amount(5), "0.05");
        assert_eq!(format_amount(0), "0.00");
        assert_eq!(format_amount(-1234), "-12.34");
        assert_eq!(format_amount(i64::MIN), "-92233720368547758.08");
    }

    #[test]
    fn evaluates_simple_expressions() {
        assert_eq!(evaluate_expression("12.34").unwrap(), "12.34");
        assert_eq!(evaluate_expression("12+3").unwrap(), "15.00");
        assert_eq!(evaluate_expression("12.34+5-2").unwrap(), "15.34");
        assert_eq!(evaluate_expression("2*3+4").unwrap(), "10.00");
        assert_eq!(evaluate_expression("2+3*4").unwrap(), "14.00", "乘法优先");
        assert_eq!(evaluate_expression("(2+3)*4").unwrap(), "20.00");
        assert_eq!(evaluate_expression("100/3").unwrap(), "33.33");
        assert_eq!(evaluate_expression("-4/2").unwrap(), "-2.00");
        assert_eq!(evaluate_expression("1,000+234").unwrap(), "1234.00", "千分位");
        assert_eq!(evaluate_expression("12 ＋ 3").unwrap(), "15.00", "全角与空格");
        assert_eq!(evaluate_expression("2×3").unwrap(), "6.00");
        assert_eq!(evaluate_expression(".5+0.5").unwrap(), "1.00");
    }

    #[test]
    fn evaluates_without_float_tail() {
        // f64 下 0.1+0.2 = 0.30000000000000004，最后按分四舍五入后应干净
        assert_eq!(evaluate_expression("0.1+0.2").unwrap(), "0.30");
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(evaluate_expression("").is_err());
        assert!(evaluate_expression("   ").is_err());
        assert!(evaluate_expression("12+").is_err());
        assert!(evaluate_expression("12/0").is_err());
        assert!(evaluate_expression("(1+2").is_err());
        assert!(evaluate_expression("abc").is_err());
        assert!(evaluate_expression("1..2").is_err());
    }
}
