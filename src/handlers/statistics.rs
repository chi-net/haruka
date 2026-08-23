use askama::Template;
use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::Html,
};
use chrono::{Duration, NaiveDate};
use rust_decimal::Decimal;
use sea_orm::EntityTrait;
use serde::Deserialize;
use std::collections::HashMap;

use crate::{
    crypto,
    entity::{account, account_detail, bill},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Deserialize)]
pub struct StatisticsQuery {
    period: Option<String>,
}

struct RankingRow {
    rank: usize,
    name: String,
    amount: String,
    count: usize,
    share: String,
}

#[derive(Template)]
#[template(path = "statistics.html")]
struct StatisticsTemplate {
    period: String,
    period_label: String,
    total_expense: String,
    expense_count: usize,
    average_expense: String,
    category_rankings: Vec<RankingRow>,
    account_rankings: Vec<RankingRow>,
}

fn start_date(period: &str, today: NaiveDate) -> Option<NaiveDate> {
    match period {
        "30d" => Some(today - Duration::days(29)),
        "365d" => Some(today - Duration::days(364)),
        _ => None,
    }
}

fn ranking_rows(values: HashMap<String, (i64, usize)>, total: i64) -> Vec<RankingRow> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .1
             .0
            .cmp(&left.1 .0)
            .then_with(|| left.0.cmp(&right.0))
    });
    values
        .into_iter()
        .enumerate()
        .map(|(index, (name, (amount, count)))| {
            let share = if total == 0 {
                Decimal::ZERO
            } else {
                (Decimal::from(amount) * Decimal::from(100) / Decimal::from(total)).round_dp(1)
            };
            RankingRow {
                rank: index + 1,
                name,
                amount: super::fmt_cents(amount),
                count,
                share: share.to_string(),
            }
        })
        .collect()
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<StatisticsQuery>,
) -> HandlerResult<Html<String>> {
    let period = match query.period.as_deref() {
        Some("365d") => "365d",
        Some("all") => "all",
        _ => "30d",
    };
    let period_label = match period {
        "365d" => "近 365 天",
        "all" => "全部时间",
        _ => "近 30 天",
    };
    let today = chrono::Local::now().date_naive();
    let start = start_date(period, today);

    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let account_names: HashMap<i64, String> = accounts
        .into_iter()
        .map(|account| {
            (
                account.id,
                super::bills::account_display_name(&dek, &account, details.get(&account.id)),
            )
        })
        .collect();

    let mut total_expense = 0i64;
    let mut expense_count = 0usize;
    let mut categories: HashMap<String, (i64, usize)> = HashMap::new();
    let mut account_values: HashMap<String, (i64, usize)> = HashMap::new();
    for bill in bill::Entity::find().all(&state.db).await.map_err(err500)? {
        if bill.kind != "expense" || start.is_some_and(|start| bill.happened_at.date() < start) {
            continue;
        }
        let amount = crypto::decrypt_cents(&dek, &bill.amount);
        total_expense = total_expense
            .checked_add(amount)
            .ok_or_else(|| err500("统计金额超出范围"))?;
        expense_count += 1;
        let category_name = crypto::decrypt_string(&dek, &bill.category);
        let category_value = categories.entry(category_name).or_default();
        category_value.0 = category_value
            .0
            .checked_add(amount)
            .ok_or_else(|| err500("统计金额超出范围"))?;
        category_value.1 += 1;
        let account_name = account_names
            .get(&bill.account_id)
            .cloned()
            .unwrap_or_else(|| "已删除账户".into());
        let account_value = account_values.entry(account_name).or_default();
        account_value.0 = account_value
            .0
            .checked_add(amount)
            .ok_or_else(|| err500("统计金额超出范围"))?;
        account_value.1 += 1;
    }
    let average = if expense_count == 0 {
        0
    } else {
        total_expense / expense_count as i64
    };
    let html = StatisticsTemplate {
        period: period.into(),
        period_label: period_label.into(),
        total_expense: super::fmt_cents(total_expense),
        expense_count,
        average_expense: super::fmt_cents(average),
        category_rankings: ranking_rows(categories, total_expense),
        account_rankings: ranking_rows(account_values, total_expense),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}
