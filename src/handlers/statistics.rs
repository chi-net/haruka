use askama::Template;
use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::Html,
};
use chrono::{Datelike, Duration, NaiveDate};
use rust_decimal::Decimal;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::{
    crypto, currency,
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

struct ExpenseOrderRow {
    rank: usize,
    id: i64,
    amount: String,
    original_amount: String,
    converted: bool,
    category: String,
    account: String,
    note: String,
    happened_at: String,
    share: String,
}

struct ExpenseOrderValue {
    id: i64,
    amount: i64,
    native_amount: i64,
    currency: String,
    category: String,
    account: String,
    note: String,
    happened_at: chrono::NaiveDateTime,
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
    median_expense: String,
    largest_expense: String,
    daily_expense: String,
    active_day_expense: String,
    expense_active_days: usize,
    calendar_days: i64,
    food_expense: String,
    food_share: String,
    income_category_rankings: Vec<RankingRow>,
    expense_category_rankings: Vec<RankingRow>,
    income_account_rankings: Vec<RankingRow>,
    expense_account_rankings: Vec<RankingRow>,
    expense_day_rankings: Vec<RankingRow>,
    expense_weekday_rankings: Vec<RankingRow>,
    largest_expenses: Vec<ExpenseOrderRow>,
    has_cashflow: bool,
    has_income: bool,
    has_expense: bool,
    trend_label: String,
    charts_json: String,
    default_currency: String,
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
    food_expenses: ChartSeries,
    expense_weekdays: ChartSeries,
    trend: TrendSeries,
}

#[derive(Serialize)]
struct TrendSeries {
    labels: Vec<String>,
    income: Vec<i64>,
    expense: Vec<i64>,
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

fn ranking_rows(
    values: HashMap<String, (i64, usize)>,
    total: i64,
    default_currency: &str,
) -> Vec<RankingRow> {
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
                amount: currency::format(amount, default_currency),
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

fn ordered_chart_series(
    labels: impl IntoIterator<Item = String>,
    values: &HashMap<String, (i64, usize)>,
) -> ChartSeries {
    let labels = labels.into_iter().collect::<Vec<_>>();
    let amounts = labels
        .iter()
        .map(|label| values.get(label).map(|value| value.0).unwrap_or_default())
        .collect();
    ChartSeries {
        labels,
        values: amounts,
    }
}

fn percentage(part: i64, total: i64) -> String {
    if total == 0 {
        return "0".into();
    }
    (Decimal::from(part) * Decimal::from(100) / Decimal::from(total))
        .round_dp(1)
        .to_string()
}

fn median(values: &mut [i64]) -> HandlerResult<i64> {
    if values.is_empty() {
        return Ok(0);
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        Ok(values[middle])
    } else {
        values[middle - 1]
            .checked_add(values[middle])
            .map(|sum| sum / 2)
            .ok_or_else(|| err500("统计金额超出范围"))
    }
}

fn trend_series(
    start_date: NaiveDate,
    end_date: NaiveDate,
    daily: &HashMap<NaiveDate, (i64, i64)>,
) -> (String, TrendSeries) {
    let calendar_days = (end_date - start_date).num_days() + 1;
    if calendar_days <= 90 {
        let mut labels = Vec::new();
        let mut income = Vec::new();
        let mut expense = Vec::new();
        let mut date = start_date;
        while date <= end_date {
            labels.push(date.format("%m-%d").to_string());
            let value = daily.get(&date).copied().unwrap_or_default();
            income.push(value.0);
            expense.push(value.1);
            let Some(next) = date.succ_opt() else {
                break;
            };
            date = next;
        }
        return (
            "每日收支趋势".into(),
            TrendSeries {
                labels,
                income,
                expense,
            },
        );
    }

    let mut monthly: HashMap<String, (i64, i64)> = HashMap::new();
    for (date, value) in daily {
        let month = date.format("%Y-%m").to_string();
        let entry = monthly.entry(month).or_default();
        entry.0 += value.0;
        entry.1 += value.1;
    }
    let mut labels = monthly.keys().cloned().collect::<Vec<_>>();
    labels.sort();
    let income = labels.iter().map(|key| monthly[key].0).collect();
    let expense = labels.iter().map(|key| monthly[key].1).collect();
    (
        "每月收支趋势".into(),
        TrendSeries {
            labels,
            income,
            expense,
        },
    )
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
    let default_currency = currency::default_currency(&state).await.map_err(err500)?;
    let currencies = accounts
        .iter()
        .map(|account| account.currency.clone())
        .collect::<Vec<_>>();
    let rates = currency::RateTable::load(&state, currencies, &default_currency, today)
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
        .iter()
        .map(|account| {
            (
                account.id,
                super::bills::account_display_name(&dek, &account, details.get(&account.id)),
            )
        })
        .collect();
    let account_currencies: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();

    let mut total_income = 0i64;
    let mut total_expense = 0i64;
    let mut income_count = 0usize;
    let mut expense_count = 0usize;
    let mut income_categories: HashMap<String, (i64, usize)> = HashMap::new();
    let mut expense_categories: HashMap<String, (i64, usize)> = HashMap::new();
    let mut income_accounts: HashMap<String, (i64, usize)> = HashMap::new();
    let mut expense_accounts: HashMap<String, (i64, usize)> = HashMap::new();
    let mut expense_days: HashMap<String, (i64, usize)> = HashMap::new();
    let mut expense_weekdays: HashMap<String, (i64, usize)> = HashMap::new();
    let mut daily_cashflow: HashMap<NaiveDate, (i64, i64)> = HashMap::new();
    let mut expense_active_dates = HashSet::new();
    let mut expense_amounts = Vec::new();
    let mut expense_orders = Vec::new();
    let mut food_expense = 0i64;
    for bill in bill::Entity::find().all(&state.db).await.map_err(err500)? {
        let date = bill.happened_at.date();
        if date < start_date || date > end_date {
            continue;
        }
        let native_amount = crypto::decrypt_cents(&dek, &bill.amount);
        let bill_currency = account_currencies
            .get(&bill.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let amount = rates
            .convert(native_amount, bill_currency)
            .map_err(err500)?;
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
            let daily = daily_cashflow.entry(date).or_default();
            daily.0 = daily
                .0
                .checked_add(amount)
                .ok_or_else(|| err500("统计金额超出范围"))?;
        } else if bill.kind == "expense" {
            total_expense = total_expense
                .checked_add(amount)
                .ok_or_else(|| err500("统计金额超出范围"))?;
            expense_count += 1;
            add_ranking_value(&mut expense_categories, category_name.clone(), amount)?;
            add_ranking_value(&mut expense_accounts, account_name.clone(), amount)?;
            add_ranking_value(
                &mut expense_days,
                date.format("%Y-%m-%d").to_string(),
                amount,
            )?;
            let weekday = match date.weekday() {
                chrono::Weekday::Mon => "星期一",
                chrono::Weekday::Tue => "星期二",
                chrono::Weekday::Wed => "星期三",
                chrono::Weekday::Thu => "星期四",
                chrono::Weekday::Fri => "星期五",
                chrono::Weekday::Sat => "星期六",
                chrono::Weekday::Sun => "星期日",
            };
            add_ranking_value(&mut expense_weekdays, weekday.into(), amount)?;
            let daily = daily_cashflow.entry(date).or_default();
            daily.1 = daily
                .1
                .checked_add(amount)
                .ok_or_else(|| err500("统计金额超出范围"))?;
            expense_active_dates.insert(date);
            expense_amounts.push(amount);
            if bill.is_food {
                food_expense = food_expense
                    .checked_add(amount)
                    .ok_or_else(|| err500("统计金额超出范围"))?;
            }
            expense_orders.push(ExpenseOrderValue {
                id: bill.id,
                amount,
                native_amount,
                currency: bill_currency.to_string(),
                category: category_name,
                account: account_name,
                note: crypto::decrypt_string(&dek, &bill.note),
                happened_at: bill.happened_at,
            });
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
    let calendar_days = (end_date - start_date).num_days() + 1;
    let daily_expense = total_expense / calendar_days.max(1);
    let active_day_expense = if expense_active_dates.is_empty() {
        0
    } else {
        total_expense / expense_active_dates.len() as i64
    };
    let median_expense = median(&mut expense_amounts)?;
    let largest_expense = expense_amounts.iter().copied().max().unwrap_or_default();
    expense_orders.sort_by(|left, right| {
        right
            .amount
            .cmp(&left.amount)
            .then_with(|| right.happened_at.cmp(&left.happened_at))
            .then_with(|| right.id.cmp(&left.id))
    });
    let largest_expenses = expense_orders
        .into_iter()
        .take(20)
        .enumerate()
        .map(|(index, item)| ExpenseOrderRow {
            rank: index + 1,
            id: item.id,
            amount: currency::format(item.amount, &default_currency),
            original_amount: currency::format(item.native_amount, &item.currency),
            converted: item.currency != default_currency,
            category: item.category,
            account: item.account,
            note: item.note,
            happened_at: item.happened_at.format("%Y-%m-%dT%H:%M").to_string(),
            share: percentage(item.amount, total_expense),
        })
        .collect();
    let weekday_labels = [
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
        "星期日",
    ];
    let expense_weekday_chart = ordered_chart_series(
        weekday_labels.iter().map(|label| (*label).to_string()),
        &expense_weekdays,
    );
    let (trend_label, trend) = trend_series(start_date, end_date, &daily_cashflow);
    let charts_json = serde_json::to_string(&StatisticsCharts {
        cashflow: ChartSeries {
            labels: vec!["收入".into(), "支出".into()],
            values: vec![total_income, total_expense],
        },
        income_categories: chart_series(&income_categories),
        expense_categories: chart_series(&expense_categories),
        food_expenses: ChartSeries {
            labels: vec!["食品支出".into(), "其他支出".into()],
            values: vec![food_expense, total_expense - food_expense],
        },
        expense_weekdays: expense_weekday_chart,
        trend,
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
        total_income: currency::format(total_income, &default_currency),
        total_expense: currency::format(total_expense, &default_currency),
        net: currency::format(net, &default_currency),
        income_count,
        expense_count,
        average_income: currency::format(average_income, &default_currency),
        average_expense: currency::format(average_expense, &default_currency),
        median_expense: currency::format(median_expense, &default_currency),
        largest_expense: currency::format(largest_expense, &default_currency),
        daily_expense: currency::format(daily_expense, &default_currency),
        active_day_expense: currency::format(active_day_expense, &default_currency),
        expense_active_days: expense_active_dates.len(),
        calendar_days,
        food_expense: currency::format(food_expense, &default_currency),
        food_share: percentage(food_expense, total_expense),
        income_category_rankings: ranking_rows(income_categories, total_income, &default_currency),
        expense_category_rankings: ranking_rows(
            expense_categories,
            total_expense,
            &default_currency,
        ),
        income_account_rankings: ranking_rows(income_accounts, total_income, &default_currency),
        expense_account_rankings: ranking_rows(expense_accounts, total_expense, &default_currency),
        expense_day_rankings: ranking_rows(expense_days, total_expense, &default_currency),
        expense_weekday_rankings: ranking_rows(expense_weekdays, total_expense, &default_currency),
        largest_expenses,
        has_cashflow: total_income > 0 || total_expense > 0,
        has_income: total_income > 0,
        has_expense: total_expense > 0,
        trend_label,
        charts_json,
        default_currency,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}
