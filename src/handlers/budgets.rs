use askama::Template;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::{Datelike, Duration};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{account, bill, budget},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

#[derive(Deserialize)]
pub struct BudgetFormData {
    #[serde(default)]
    daily_amount: String,
    #[serde(default)]
    weekly_amount: String,
    #[serde(default)]
    monthly_amount: String,
}

pub(crate) struct BudgetStatus {
    pub(crate) label: String,
    pub(crate) period_label: String,
    pub(crate) budget: String,
    pub(crate) used: String,
    pub(crate) remaining: String,
    pub(crate) percent: String,
    pub(crate) bar_percent: i64,
    pub(crate) over_budget: bool,
    pub(crate) near_limit: bool,
}

#[derive(Template)]
#[template(path = "budgets.html")]
struct BudgetsTemplate {
    daily_amount: String,
    weekly_amount: String,
    monthly_amount: String,
    statuses: Vec<BudgetStatus>,
    default_currency: String,
}

fn parse_budget(value: &str, label: &str) -> HandlerResult<Option<i64>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let decimal = Decimal::from_str(value)
        .map_err(|_| bad_request(format!("{label}格式不正确")))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request(format!("{label}必须大于 0；留空可停用")));
    }
    let cents = (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request(format!("{label}超出范围")))?;
    Ok(Some(cents))
}

fn encrypt_optional(dek: &crypto::Dek, amount: Option<i64>) -> String {
    amount
        .map(|amount| crypto::encrypt_cents(dek, amount))
        .unwrap_or_default()
}

fn decrypt_optional(dek: &crypto::Dek, amount: &str) -> Option<i64> {
    (!amount.is_empty()).then(|| crypto::decrypt_cents(dek, amount))
}

fn input_amount(amount: Option<i64>) -> String {
    amount.map(super::fmt_cents).unwrap_or_default()
}

fn percentage(used: i64, limit: i64) -> Decimal {
    (Decimal::from(used) * Decimal::from(100) / Decimal::from(limit)).round_dp(1)
}

fn make_status(
    label: &str,
    period_label: String,
    limit: i64,
    used: i64,
    default_currency: &str,
) -> BudgetStatus {
    let percent = percentage(used, limit);
    let percent_integer = percent.round().to_i64().unwrap_or(i64::MAX);
    let remaining = limit.saturating_sub(used);
    BudgetStatus {
        label: label.into(),
        period_label,
        budget: currency::format(limit, default_currency),
        used: currency::format(used, default_currency),
        remaining: currency::format(remaining.saturating_abs(), default_currency),
        percent: percent.to_string(),
        bar_percent: percent_integer.clamp(0, 100),
        over_budget: used > limit,
        near_limit: used >= limit.saturating_mul(4) / 5,
    }
}

pub(crate) async fn current_statuses(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<Vec<BudgetStatus>> {
    let Some(config) = budget::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
    else {
        return Ok(Vec::new());
    };
    let daily = decrypt_optional(dek, &config.daily_amount);
    let weekly = decrypt_optional(dek, &config.weekly_amount);
    let monthly = decrypt_optional(dek, &config.monthly_amount);
    if daily.is_none() && weekly.is_none() && monthly.is_none() {
        return Ok(Vec::new());
    }

    let today = chrono::Local::now().date_naive();
    let week_start = today - Duration::days(i64::from(today.weekday().num_days_from_monday()));
    let month_start = today
        .with_day(1)
        .ok_or_else(|| err500("无法计算本月起始日"))?;
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let default_currency = currency::default_currency(state).await.map_err(err500)?;
    let account_currencies: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();
    let currencies = accounts
        .iter()
        .map(|account| account.currency.clone())
        .collect::<Vec<_>>();
    let rates = currency::RateTable::load(state, currencies, &default_currency, today)
        .await
        .map_err(err500)?;
    let mut daily_used = 0i64;
    let mut weekly_used = 0i64;
    let mut monthly_used = 0i64;
    for item in bill::Entity::find().all(&state.db).await.map_err(err500)? {
        if item.kind != "expense" {
            continue;
        }
        let date = item.happened_at.date();
        if date > today || date < month_start.min(week_start).min(today) {
            continue;
        }
        let native_amount = crypto::decrypt_cents(dek, &item.amount);
        let bill_currency = account_currencies
            .get(&item.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let amount = rates
            .convert(native_amount, bill_currency)
            .map_err(err500)?;
        if date == today {
            daily_used = daily_used
                .checked_add(amount)
                .ok_or_else(|| err500("预算金额超出范围"))?;
        }
        if date >= week_start {
            weekly_used = weekly_used
                .checked_add(amount)
                .ok_or_else(|| err500("预算金额超出范围"))?;
        }
        if date >= month_start {
            monthly_used = monthly_used
                .checked_add(amount)
                .ok_or_else(|| err500("预算金额超出范围"))?;
        }
    }

    let mut statuses = Vec::new();
    if let Some(limit) = daily {
        statuses.push(make_status(
            "今日预算",
            today.format("%Y-%m-%d").to_string(),
            limit,
            daily_used,
            &default_currency,
        ));
    }
    if let Some(limit) = weekly {
        let week_end = week_start + Duration::days(6);
        statuses.push(make_status(
            "本周预算",
            format!(
                "{} 至 {}",
                week_start.format("%m-%d"),
                week_end.format("%m-%d")
            ),
            limit,
            weekly_used,
            &default_currency,
        ));
    }
    if let Some(limit) = monthly {
        statuses.push(make_status(
            "本月预算",
            today.format("%Y 年 %m 月").to_string(),
            limit,
            monthly_used,
            &default_currency,
        ));
    }
    Ok(statuses)
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let config = budget::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let daily_amount = config
        .as_ref()
        .and_then(|item| decrypt_optional(&dek, &item.daily_amount));
    let weekly_amount = config
        .as_ref()
        .and_then(|item| decrypt_optional(&dek, &item.weekly_amount));
    let monthly_amount = config
        .as_ref()
        .and_then(|item| decrypt_optional(&dek, &item.monthly_amount));
    let html = BudgetsTemplate {
        daily_amount: input_amount(daily_amount),
        weekly_amount: input_amount(weekly_amount),
        monthly_amount: input_amount(monthly_amount),
        statuses: current_statuses(&state, &dek).await?,
        default_currency: currency::default_currency(&state).await.map_err(err500)?,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<BudgetFormData>,
) -> HandlerResult<Redirect> {
    let daily = parse_budget(&form.daily_amount, "日预算")?;
    let weekly = parse_budget(&form.weekly_amount, "周预算")?;
    let monthly = parse_budget(&form.monthly_amount, "月预算")?;
    if let Some(config) = budget::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        let mut active = config.into_active_model();
        active.daily_amount = Set(encrypt_optional(&dek, daily));
        active.weekly_amount = Set(encrypt_optional(&dek, weekly));
        active.monthly_amount = Set(encrypt_optional(&dek, monthly));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&state.db).await.map_err(err500)?;
    } else {
        budget::ActiveModel {
            id: Set(1),
            daily_amount: Set(encrypt_optional(&dek, daily)),
            weekly_amount: Set(encrypt_optional(&dek, weekly)),
            monthly_amount: Set(encrypt_optional(&dek, monthly)),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(Redirect::to("/budgets"))
}
