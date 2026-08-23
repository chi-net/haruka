pub mod accounts;
pub mod auth;
pub mod bills;
pub mod dashboard;
pub mod debts;
pub mod settings;
pub mod statistics;
pub mod subscriptions;
pub mod transfers;

use askama::Template;
use axum::{
    body::to_bytes,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{Html, IntoResponse, Response},
};

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    message: String,
}

pub async fn render_server_error(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
        return response;
    }
    let message = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|body| String::from_utf8(body.to_vec()).ok())
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| "服务端发生未知错误".into());
    eprintln!("请求处理失败: {message}");
    let html = ErrorTemplate { message }
        .render()
        .unwrap_or_else(|_| "服务端发生错误，且错误页面渲染失败".into());
    (StatusCode::INTERNAL_SERVER_ERROR, Html(html)).into_response()
}

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
