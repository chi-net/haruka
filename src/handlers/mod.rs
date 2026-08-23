pub mod accounts;
pub mod auth;
pub mod bills;
pub mod debts;
pub mod settings;
pub mod transfers;

/// 将分格式化为 "12.34" 形式的字符串
pub fn fmt_cents(cents: i64) -> String {
    rust_decimal::Decimal::new(cents, 2).to_string()
}
