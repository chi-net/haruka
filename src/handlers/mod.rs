pub mod accounts;
pub mod auth;
pub mod bills;
pub mod dashboard;
pub mod debts;
pub mod installments;
pub mod passkeys;
pub mod settings;
pub mod statistics;
pub mod subscriptions;
pub mod transfers;

use askama::Template;
use axum::{
    body::to_bytes,
    extract::Request,
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    status: u16,
    title: String,
    message: String,
}

#[derive(Serialize)]
struct ErrorPayload {
    ok: bool,
    status: u16,
    error: String,
}

fn error_message(content_type: &str, body: &[u8], status: StatusCode) -> String {
    let raw = String::from_utf8_lossy(body).trim().to_string();
    if content_type.contains("application/json") {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
            for key in ["error", "message", "detail"] {
                if let Some(message) = value.get(key).and_then(|item| item.as_str()) {
                    if !message.trim().is_empty() {
                        return message.trim().to_string();
                    }
                }
            }
        }
    }
    if !raw.is_empty() {
        return raw;
    }
    status
        .canonical_reason()
        .map(|reason| format!("请求失败：{reason}"))
        .unwrap_or_else(|| "请求处理失败".to_string())
}

/// 把所有失败响应统一转换成可消费的错误协议：脚本/htmx 请求返回 JSON，
/// 普通浏览器导航返回完整 HTML 错误页。原始错误详情不会被吞掉。
pub async fn render_error_response(request: Request, next: Next) -> Response {
    let accepts_json = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"));
    let sends_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"));
    let is_htmx = request
        .headers()
        .get("hx-request")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let response = next.run(request).await;
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let message = error_message(&content_type, &body, status);
    if status.is_server_error() {
        eprintln!("请求处理失败（{}）: {message}", status.as_u16());
    }

    if accepts_json || sends_json || is_htmx {
        let mut response = (
            status,
            Json(ErrorPayload {
                ok: false,
                status: status.as_u16(),
                error: message,
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }

    let title = if status.is_server_error() {
        "服务器处理失败"
    } else if status == StatusCode::NOT_FOUND {
        "没有找到请求的内容"
    } else {
        "操作失败"
    };
    let html = ErrorTemplate {
        status: status.as_u16(),
        title: title.to_string(),
        message,
    }
    .render()
    .unwrap_or_else(|_| "请求失败，且错误页面渲染失败".into());
    let mut response = (status, Html(html)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../../static/app.css"),
    )
}

/// 将分格式化为 "12.34" 形式的字符串
pub fn fmt_cents(cents: i64) -> String {
    rust_decimal::Decimal::new(cents, 2).to_string()
}

pub fn transfer_to_cents(
    dek: &crate::crypto::Dek,
    transfer: &crate::entity::transfer::Model,
) -> i64 {
    if transfer.to_amount.is_empty() {
        crate::crypto::decrypt_cents(dek, &transfer.amount)
    } else {
        crate::crypto::decrypt_cents(dek, &transfer.to_amount)
    }
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
