use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Html,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::{currency, AppState};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

#[derive(Default, Deserialize)]
pub struct ConverterQuery {
    #[serde(default)]
    amount: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    date: String,
}

#[derive(Template)]
#[template(path = "currency_converter.html")]
struct CurrencyConverterTemplate {
    currencies: &'static [currency::CurrencyOption],
    amount: String,
    from: String,
    to: String,
    date: String,
    result: String,
    rate: String,
    rate_date: String,
    fetched_at: String,
    same_currency: bool,
}

pub async fn converter(
    State(state): State<AppState>,
    Query(mut query): Query<ConverterQuery>,
) -> HandlerResult<Html<String>> {
    let today = chrono::Local::now().date_naive();
    let default_currency = currency::default_currency(&state).await.map_err(err500)?;
    if query.amount.trim().is_empty() {
        query.amount = "1.00".into();
    }
    if query.from.trim().is_empty() {
        query.from = default_currency.clone();
    } else {
        query.from = query.from.trim().to_uppercase();
    }
    if query.to.trim().is_empty() {
        query.to = if default_currency == "USD" {
            "CNY"
        } else {
            "USD"
        }
        .into();
    } else {
        query.to = query.to.trim().to_uppercase();
    }
    if !currency::valid(&query.from) || !currency::valid(&query.to) {
        return Err(bad_request("请选择支持的换算货币"));
    }
    let date = if query.date.trim().is_empty() {
        today
    } else {
        NaiveDate::parse_from_str(query.date.trim(), "%Y-%m-%d")
            .map_err(|_| bad_request("换算日期格式不正确"))?
    };
    if date > today {
        return Err(bad_request("不能查询未来汇率"));
    }
    let amount =
        Decimal::from_str(query.amount.trim()).map_err(|_| bad_request("换算金额格式不正确"))?;
    if amount < Decimal::ZERO {
        return Err(bad_request("换算金额不能小于 0"));
    }
    let info = currency::rate_with_info(&state, &query.from, &query.to, date)
        .await
        .map_err(err500)?;
    let converted = (amount * info.rate).round_dp(4).normalize();
    let fetched_at = info
        .fetched_at
        .map(|time| time.format("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default();
    let html = CurrencyConverterTemplate {
        currencies: currency::CURRENCIES,
        amount: query.amount,
        from: query.from.clone(),
        to: query.to.clone(),
        date: date.format("%Y-%m-%d").to_string(),
        result: format!("{} {}", query.to, converted),
        rate: format!(
            "1 {} = {} {}",
            query.from,
            info.rate.round_dp(8).normalize(),
            query.to
        ),
        rate_date: info.rate_date.format("%Y-%m-%d").to_string(),
        fetched_at,
        same_currency: query.from == query.to,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}
