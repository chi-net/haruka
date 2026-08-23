use askama::Template;
use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::Html,
};
use chrono::{Duration, NaiveDate};
use rust_decimal::Decimal;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    period: String,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
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
    start_date: String,
    end_date: String,
    period_label: String,
    total_income: String,
    total_expense: String,
    net: String,
    income_count: usize,
    expense_count: usize,
    average_income: String,
    average_expense: String,
    income_category_rankings: Vec<RankingRow>,
    expense_category_rankings: Vec<RankingRow>,
    income_account_rankings: Vec<RankingRow>,
    expense_account_rankings: Vec<RankingRow>,
    has_cashflow: bool,
    has_income: bool,
    has_expense: bool,
    charts_json: String,
}

#[derive(Serialize)]
struct ChartSeries {
    labels: Vec<String>,
    values: Vec<i64>,
}

#[derive(Serialize)]
struct StatisticsCharts {
    cashflow: ChartSeries,
    income_categories: ChartSeries,
    expense_categories: ChartSeries,
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

fn parse_date(value: &str, fallback: NaiveDate, label: &str) -> HandlerResult<NaiveDate> {
    if value.trim().is_empty() {
        return Ok(fallback);
    }
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| bad_request(&format!("{label}格式不正确")))
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

fn chart_series(values: &HashMap<String, (i64, usize)>) -> ChartSeries {
    let mut values = values.iter().collect::<Vec<_>>();
    values.sort_by(|left, right| right.1 .0.cmp(&left.1 .0));
    ChartSeries {
        labels: values.iter().map(|(name, _)| (*name).clone()).collect(),
        values: values.iter().map(|(_, value)| value.0).collect(),
    }
}

fn add_ranking_value(
    values: &mut HashMap<String, (i64, usize)>,
    name: String,
    amount: i64,
) -> HandlerResult<()> {
    let value = values.entry(name).or_default();
    value.0 = value
        .0
        .checked_add(amount)
        .ok_or_else(|| err500("统计金额超出范围"))?;
    value.1 += 1;
    Ok(())
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<StatisticsQuery>,
) -> HandlerResult<Html<String>> {
    let today = chrono::Local::now().date_naive();
    let preset = match query.period.as_str() {
        "7d" => Some(("7d", 7)),
        "14d" => Some(("14d", 14)),
        "30d" => Some(("30d", 30)),
        "90d" => Some(("90d", 90)),
        "365d" => Some(("365d", 365)),
        _ if query.start_date.trim().is_empty() && query.end_date.trim().is_empty() => {
            Some(("30d", 30))
        }
        _ => None,
    };
    let (period, start_date, end_date) = if let Some((period, days)) = preset {
        (period, today - Duration::days(days - 1), today)
    } else {
        (
            "custom",
            parse_date(&query.start_date, today - Duration::days(29), "开始日期")?,
            parse_date(&query.end_date, today, "结束日期")?,
        )
    };
    if start_date > end_date {
        return Err(bad_request("开始日期不能晚于结束日期"));
    }
    let period_label = format!(
        "{} 至 {}",
        start_date.format("%Y-%m-%d"),
        end_date.format("%Y-%m-%d")
    );

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

    let mut total_income = 0i64;
    let mut total_expense = 0i64;
    let mut income_count = 0usize;
    let mut expense_count = 0usize;
    let mut income_categories: HashMap<String, (i64, usize)> = HashMap::new();
    let mut expense_categories: HashMap<String, (i64, usize)> = HashMap::new();
    let mut income_accounts: HashMap<String, (i64, usize)> = HashMap::new();
    let mut expense_accounts: HashMap<String, (i64, usize)> = HashMap::new();
    for bill in bill::Entity::find().all(&state.db).await.map_err(err500)? {
        let date = bill.happened_at.date();
        if date < start_date || date > end_date {
            continue;
        }
        let amount = crypto::decrypt_cents(&dek, &bill.amount);
        let category_name = crypto::decrypt_string(&dek, &bill.category);
        let account_name = account_names
            .get(&bill.account_id)
            .cloned()
            .unwrap_or_else(|| "已删除账户".into());
        if bill.kind == "income" {
            total_income = total_income
                .checked_add(amount)
                .ok_or_else(|| err500("统计金额超出范围"))?;
            income_count += 1;
            add_ranking_value(&mut income_categories, category_name, amount)?;
            add_ranking_value(&mut income_accounts, account_name, amount)?;
        } else if bill.kind == "expense" {
            total_expense = total_expense
                .checked_add(amount)
                .ok_or_else(|| err500("统计金额超出范围"))?;
            expense_count += 1;
            add_ranking_value(&mut expense_categories, category_name, amount)?;
            add_ranking_value(&mut expense_accounts, account_name, amount)?;
        }
    }
    let average_income = if income_count == 0 {
        0
    } else {
        total_income / income_count as i64
    };
    let average_expense = if expense_count == 0 {
        0
    } else {
        total_expense / expense_count as i64
    };
    let net = total_income
        .checked_sub(total_expense)
        .ok_or_else(|| err500("统计金额超出范围"))?;
    let charts_json = serde_json::to_string(&StatisticsCharts {
        cashflow: ChartSeries {
            labels: vec!["收入".into(), "支出".into()],
            values: vec![total_income, total_expense],
        },
        income_categories: chart_series(&income_categories),
        expense_categories: chart_series(&expense_categories),
    })
    .map_err(err500)?
    .replace('<', "\\u003c")
    .replace('>', "\\u003e")
    .replace('&', "\\u0026");
    let html = StatisticsTemplate {
        period: period.into(),
        start_date: start_date.format("%Y-%m-%d").to_string(),
        end_date: end_date.format("%Y-%m-%d").to_string(),
        period_label,
        total_income: super::fmt_cents(total_income),
        total_expense: super::fmt_cents(total_expense),
        net: super::fmt_cents(net),
        income_count,
        expense_count,
        average_income: super::fmt_cents(average_income),
        average_expense: super::fmt_cents(average_expense),
        income_category_rankings: ranking_rows(income_categories, total_income),
        expense_category_rankings: ranking_rows(expense_categories, total_expense),
        income_account_rankings: ranking_rows(income_accounts, total_income),
        expense_account_rankings: ranking_rows(expense_accounts, total_expense),
        has_cashflow: total_income > 0 || total_expense > 0,
        has_income: total_income > 0,
        has_expense: total_expense > 0,
        charts_json,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}
