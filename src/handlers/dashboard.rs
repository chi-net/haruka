use askama::Template;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{Html, Redirect},
};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use rust_decimal::Decimal;
use sea_orm::{EntityTrait, QueryOrder};
use serde::Serialize;
use std::collections::HashMap;

use crate::{
    crypto,
    entity::{account, account_detail, bill, category, debt_person, debt_record, transfer},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const DISPLAY_FMT: &str = "%Y-%m-%d %H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

struct AccountOption {
    id: i64,
    name: String,
}

struct AccountSummary {
    name: String,
    kind: String,
    balance: String,
}

struct PersonOption {
    id: i64,
    name: String,
}

struct CategoryOption {
    kind: String,
    name: String,
}

struct ActivityRow {
    happened_at: String,
    kind: String,
    detail: String,
    note: String,
    amount: String,
    money_class: String,
    delete_action: String,
    delete_confirm: String,
    sort_key: chrono::NaiveDateTime,
}

#[derive(Serialize)]
struct ReportSeries {
    labels: Vec<String>,
    income: Vec<i64>,
    expense: Vec<i64>,
}

#[derive(Serialize)]
struct Reports {
    daily: ReportSeries,
    weekly: ReportSeries,
    monthly: ReportSeries,
    yearly: ReportSeries,
}

struct BillValue {
    happened_at: NaiveDateTime,
    kind: String,
    amount: i64,
    is_food: bool,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    accounts: Vec<AccountOption>,
    account_summaries: Vec<AccountSummary>,
    people: Vec<PersonOption>,
    categories: Vec<CategoryOption>,
    activities: Vec<ActivityRow>,
    happened_at: String,
    net_assets: String,
    month_income: String,
    month_expense: String,
    receivable: String,
    payable: String,
    engel_coefficient: String,
    food_expense: String,
    reports_json: String,
}

fn account_kind_label(kind: &str) -> &'static str {
    match kind {
        "payment" => "支付",
        "bank" => "银行",
        "stored_value" => "储值卡",
        "credit_card" => "信用卡",
        "credit_service" => "信贷服务",
        "investment" => "投资",
        _ => "其他",
    }
}

fn debt_kind_label(kind: &str) -> &'static str {
    match kind {
        "lend" => "借出",
        "borrow" => "借入",
        "repayment_received" => "收回还款",
        "repayment_paid" => "归还借款",
        _ => "借还",
    }
}

fn add_value(series: &mut ReportSeries, index: usize, bill: &BillValue) -> HandlerResult<()> {
    let target = if bill.kind == "income" {
        &mut series.income[index]
    } else {
        &mut series.expense[index]
    };
    *target = target
        .checked_add(bill.amount)
        .ok_or_else(|| err500("报表金额超出范围"))?;
    Ok(())
}

fn build_date_series(
    today: NaiveDate,
    days: i64,
    bills: &[BillValue],
) -> HandlerResult<ReportSeries> {
    let dates = (0..days)
        .map(|offset| today - Duration::days(days - 1 - offset))
        .collect::<Vec<_>>();
    let mut series = ReportSeries {
        labels: dates
            .iter()
            .map(|date| date.format("%m-%d").to_string())
            .collect(),
        income: vec![0; dates.len()],
        expense: vec![0; dates.len()],
    };
    let indexes: HashMap<NaiveDate, usize> = dates
        .iter()
        .enumerate()
        .map(|(index, date)| (*date, index))
        .collect();
    for bill in bills {
        if let Some(index) = indexes.get(&bill.happened_at.date()) {
            add_value(&mut series, *index, bill)?;
        }
    }
    Ok(series)
}

fn build_reports(today: NaiveDate, bills: &[BillValue]) -> HandlerResult<Reports> {
    let mut daily = ReportSeries {
        labels: (0..24).map(|hour| format!("{hour:02}:00")).collect(),
        income: vec![0; 24],
        expense: vec![0; 24],
    };
    for bill in bills {
        if bill.happened_at.date() == today {
            add_value(&mut daily, bill.happened_at.hour() as usize, bill)?;
        }
    }
    Ok(Reports {
        daily,
        weekly: build_date_series(today, 7, bills)?,
        monthly: build_date_series(today, 30, bills)?,
        yearly: build_date_series(today, 365, bills)?,
    })
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
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
        .iter()
        .map(|account| {
            (
                account.id,
                super::bills::account_display_name(&dek, account, details.get(&account.id)),
            )
        })
        .collect();
    let account_options = accounts
        .iter()
        .map(|account| AccountOption {
            id: account.id,
            name: account_names.get(&account.id).cloned().unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let mut net_assets = 0i64;
    let mut account_summaries = Vec::with_capacity(accounts.len());
    for account in &accounts {
        let balance = super::accounts::current_balance(&state, &dek, account.id).await?;
        net_assets = net_assets
            .checked_add(balance)
            .ok_or_else(|| err500("资产金额超出范围"))?;
        account_summaries.push(AccountSummary {
            name: account_names.get(&account.id).cloned().unwrap_or_default(),
            kind: account_kind_label(&account.kind).into(),
            balance: super::fmt_cents(balance),
        });
    }

    let people = debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let person_names: HashMap<i64, String> = people
        .iter()
        .map(|person| (person.id, crypto::decrypt_string(&dek, &person.name)))
        .collect();
    let people_options = people
        .iter()
        .map(|person| PersonOption {
            id: person.id,
            name: person_names.get(&person.id).cloned().unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let categories = category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|category| CategoryOption {
            kind: category.kind,
            name: crypto::decrypt_string(&dek, &category.name),
        })
        .collect::<Vec<_>>();

    let debt_records = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut receivable = 0i64;
    let mut payable = 0i64;
    let mut activities = Vec::new();
    for record in debt_records {
        let amount = crypto::decrypt_cents(&dek, &record.amount);
        match record.kind.as_str() {
            "lend" => {
                receivable = receivable
                    .checked_add(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            "repayment_received" => {
                receivable = receivable
                    .checked_sub(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            "borrow" => {
                payable = payable
                    .checked_add(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            "repayment_paid" => {
                payable = payable
                    .checked_sub(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            _ => {}
        }
        let incoming = matches!(record.kind.as_str(), "borrow" | "repayment_received");
        activities.push(ActivityRow {
            happened_at: record.happened_at.format(DISPLAY_FMT).to_string(),
            kind: debt_kind_label(&record.kind).into(),
            detail: format!(
                "{} · {}",
                person_names
                    .get(&record.person_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除对象".into()),
                account_names
                    .get(&record.account_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除账户".into())
            ),
            amount: format!(
                "{}{}",
                if incoming { "+" } else { "-" },
                super::fmt_cents(amount)
            ),
            money_class: if incoming {
                "text-green-600"
            } else {
                "text-red-600"
            }
            .into(),
            note: crypto::decrypt_string(&dek, &record.note),
            delete_action: format!("/debts/{}/delete", record.id),
            delete_confirm: "确认删除这条借还记录？".into(),
            sort_key: record.happened_at,
        });
    }

    for transfer in transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        activities.push(ActivityRow {
            happened_at: transfer.happened_at.format(DISPLAY_FMT).to_string(),
            kind: "转账".into(),
            detail: format!(
                "{} → {}",
                account_names
                    .get(&transfer.from_account_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除账户".into()),
                account_names
                    .get(&transfer.to_account_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除账户".into())
            ),
            amount: super::fmt_cents(crypto::decrypt_cents(&dek, &transfer.amount)),
            money_class: "text-blue-600".into(),
            note: crypto::decrypt_string(&dek, &transfer.note),
            delete_action: format!("/transfers/{}/delete", transfer.id),
            delete_confirm: "确认删除这条转账？".into(),
            sort_key: transfer.happened_at,
        });
    }
    activities.sort_by(|left, right| right.sort_key.cmp(&left.sort_key));

    let today = chrono::Local::now().date_naive();
    let bill_values = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| BillValue {
            happened_at: bill.happened_at,
            kind: bill.kind,
            amount: crypto::decrypt_cents(&dek, &bill.amount),
            is_food: bill.is_food,
        })
        .collect::<Vec<_>>();
    let mut month_income = 0i64;
    let mut month_expense = 0i64;
    let mut food_expense = 0i64;
    for bill in &bill_values {
        let date = bill.happened_at.date();
        if date.year() == today.year() && date.month() == today.month() {
            if bill.kind == "income" {
                month_income = month_income
                    .checked_add(bill.amount)
                    .ok_or_else(|| err500("报表金额超出范围"))?;
            } else {
                month_expense = month_expense
                    .checked_add(bill.amount)
                    .ok_or_else(|| err500("报表金额超出范围"))?;
                if bill.is_food {
                    food_expense = food_expense
                        .checked_add(bill.amount)
                        .ok_or_else(|| err500("报表金额超出范围"))?;
                }
            }
        }
    }
    let engel_coefficient = if month_expense == 0 {
        "—".into()
    } else {
        format!(
            "{}%",
            (Decimal::from(food_expense) * Decimal::from(100) / Decimal::from(month_expense))
                .round_dp(1)
        )
    };
    let reports_json =
        serde_json::to_string(&build_reports(today, &bill_values)?).map_err(err500)?;
    let html = DashboardTemplate {
        accounts: account_options,
        account_summaries,
        people: people_options,
        categories,
        activities,
        happened_at: chrono::Local::now()
            .naive_local()
            .format(TIME_FMT)
            .to_string(),
        net_assets: super::fmt_cents(net_assets),
        month_income: super::fmt_cents(month_income),
        month_expense: super::fmt_cents(month_expense),
        receivable: super::fmt_cents(receivable),
        payable: super::fmt_cents(payable),
        engel_coefficient,
        food_expense: super::fmt_cents(food_expense),
        reports_json,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn redirect() -> Redirect {
    Redirect::to("/dashboard")
}
