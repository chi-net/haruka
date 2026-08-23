pub mod accounts;
pub mod auth;
pub mod bills;
pub mod debts;
pub mod settings;
pub mod subscriptions;
pub mod transfers;

/// 将分格式化为 "12.34" 形式的字符串
pub fn fmt_cents(cents: i64) -> String {
    rust_decimal::Decimal::new(cents, 2).to_string()
}

pub fn mask_card_number(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let mut suffix: Vec<char> = value.chars().rev().take(4).collect();
    suffix.reverse();
    format!("•••• {}", suffix.into_iter().collect::<String>())
}

pub fn mask_account_username(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 5 {
        if chars.len() == 1 {
            return format!("{}•••", chars[0]);
        }
        return format!("{}•••{}", chars[0], chars[chars.len() - 1]);
    }
    let prefix: String = chars[..3].iter().collect();
    let suffix: String = chars[chars.len() - 2..].iter().collect();
    format!("{prefix}•••{suffix}")
}
