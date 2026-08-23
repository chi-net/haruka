use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::{Months, NaiveDate};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{
        account, account_detail, bill, category, installment_item, installment_plan, transfer,
    },
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

pub(crate) struct NewPlan {
    pub term_months: i32,
    pub method: String,
    pub annual_rate_bps: i64,
    pub fee: i64,
    pub first_due_date: NaiveDate,
}

struct ScheduleAmount {
    sequence: i32,
    due_date: NaiveDate,
    principal: i64,
    interest: i64,
    fee: i64,
    total: i64,
}

struct InstallmentRow {
    id: i64,
    title: String,
    account_name: String,
    principal: String,
    term: String,
    method: String,
    annual_rate: String,
    total_repayment: String,
    remaining: String,
    next_due: String,
    progress: String,
    overdue_count: usize,
    method_code: String,
    next_due_date: Option<NaiveDate>,
    completed: bool,
}

struct ScheduleRow {
    id: i64,
    sequence: i32,
    due_date: String,
    principal: String,
    interest: String,
    fee: String,
    total: String,
    paid: bool,
    overdue: bool,
    repayment_account_name: String,
}

struct RepaymentAccountOption {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "installments.html")]
struct InstallmentsTemplate {
    page_heading: String,
    advanced_search: bool,
    search_action: String,
    plans: Vec<InstallmentRow>,
    mode: String,
    keyword: String,
    status: String,
    method: String,
    start_date: String,
    end_date: String,
    has_filters: bool,
    page: usize,
    per_page: usize,
    total_pages: usize,
    total_records: usize,
}

#[derive(Template)]
#[template(path = "installment_detail.html")]
struct InstallmentDetailTemplate {
    title: String,
    account_name: String,
    currency: String,
    principal: String,
    term: String,
    method: String,
    annual_rate: String,
    total_interest: String,
    total_fee: String,
    total_repayment: String,
    first_due_date: String,
    schedule: Vec<ScheduleRow>,
    repayment_accounts: Vec<RepaymentAccountOption>,
    plan_id: i64,
    page: usize,
    per_page: usize,
    total_pages: usize,
    total_records: usize,
}

#[derive(Deserialize)]
pub struct PaidFormData {
    paid: bool,
    #[serde(default)]
    repayment_account_id: Option<i64>,
}

#[derive(Default, Deserialize)]
pub struct InstallmentsQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
}

#[derive(Default, Deserialize)]
pub struct InstallmentDetailQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
}

pub(crate) fn parse_input(
    term: &str,
    method: &str,
    annual_rate: &str,
    fee: &str,
    first_due_date: &str,
) -> HandlerResult<NewPlan> {
    let term_months: i32 = term
        .trim()
        .parse()
        .map_err(|_| bad_request("分期期限无效"))?;
    if !matches!(
        term_months,
        3 | 6 | 9 | 12 | 24 | 36 | 60 | 120 | 180 | 240 | 360
    ) {
        return Err(bad_request("分期期限无效"));
    }
    if !matches!(method, "equal_payment" | "equal_principal" | "flat") {
        return Err(bad_request("还款方式无效"));
    }
    let rate = Decimal::from_str(annual_rate.trim())
        .map_err(|_| bad_request("年利率格式不正确"))?
        .round_dp(2);
    if rate < Decimal::ZERO || rate > Decimal::from(100) {
        return Err(bad_request("年利率必须在 0% 到 100% 之间"));
    }
    let annual_rate_bps = (rate * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("年利率超出范围"))?;
    let fee_decimal = Decimal::from_str(fee.trim())
        .map_err(|_| bad_request("总手续费格式不正确"))?
        .round_dp(2);
    if fee_decimal < Decimal::ZERO {
        return Err(bad_request("总手续费不能小于 0"));
    }
    let fee = (fee_decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("总手续费超出范围"))?;
    let first_due_date = NaiveDate::parse_from_str(first_due_date.trim(), "%Y-%m-%d")
        .map_err(|_| bad_request("首期还款日格式不正确"))?;
    Ok(NewPlan {
        term_months,
        method: method.into(),
        annual_rate_bps,
        fee,
        first_due_date,
    })
}

fn checked_add(left: i64, right: i64) -> HandlerResult<i64> {
    left.checked_add(right)
        .ok_or_else(|| bad_request("分期金额超出范围"))
}

fn split_amount(total: i64, count: i32, index: i32) -> i64 {
    let base = total / i64::from(count);
    let remainder = total % i64::from(count);
    base + i64::from(index <= remainder as i32)
}

fn schedule(principal: i64, plan: &NewPlan) -> HandlerResult<Vec<ScheduleAmount>> {
    let count = plan.term_months;
    let monthly_rate = Decimal::from(plan.annual_rate_bps) / Decimal::from(10_000 * 12);
    let fixed_payment = if monthly_rate.is_zero() {
        Decimal::from(principal) / Decimal::from(count)
    } else {
        let mut factor = Decimal::ONE;
        for _ in 0..count {
            factor *= Decimal::ONE + monthly_rate;
        }
        Decimal::from(principal) * monthly_rate * factor / (factor - Decimal::ONE)
    };
    let flat_total_interest =
        (Decimal::from(principal) * Decimal::from(plan.annual_rate_bps) * Decimal::from(count)
            / Decimal::from(10_000 * 12))
        .round()
        .to_i64()
        .ok_or_else(|| bad_request("利息金额超出范围"))?;

    let mut rows = Vec::with_capacity(count as usize);
    let mut remaining = principal;
    for sequence in 1..=count {
        let due_date = plan
            .first_due_date
            .checked_add_months(Months::new((sequence - 1) as u32))
            .ok_or_else(|| bad_request("还款日期超出范围"))?;
        let (principal_part, interest) = match plan.method.as_str() {
            "equal_principal" => {
                let principal_part = split_amount(principal, count, sequence).min(remaining);
                let interest = (Decimal::from(remaining) * monthly_rate)
                    .round()
                    .to_i64()
                    .ok_or_else(|| bad_request("利息金额超出范围"))?;
                (principal_part, interest)
            }
            "flat" => (
                split_amount(principal, count, sequence).min(remaining),
                split_amount(flat_total_interest, count, sequence),
            ),
            _ => {
                let interest = (Decimal::from(remaining) * monthly_rate)
                    .round()
                    .to_i64()
                    .ok_or_else(|| bad_request("利息金额超出范围"))?;
                let principal_part = if sequence == count {
                    remaining
                } else {
                    (fixed_payment.round().to_i64().unwrap_or_default() - interest)
                        .max(0)
                        .min(remaining)
                };
                (principal_part, interest)
            }
        };
        remaining = remaining
            .checked_sub(principal_part)
            .ok_or_else(|| bad_request("分期本金超出范围"))?;
        let fee = split_amount(plan.fee, count, sequence);
        let total = checked_add(checked_add(principal_part, interest)?, fee)?;
        rows.push(ScheduleAmount {
            sequence,
            due_date,
            principal: principal_part,
            interest,
            fee,
            total,
        });
    }
    if remaining != 0 {
        return Err(bad_request("无法生成完整的分期本金计划"));
    }
    Ok(rows)
}

pub(crate) async fn create_plan<C: ConnectionTrait>(
    db: &C,
    dek: &crypto::Dek,
    bill_id: i64,
    account_id: i64,
    principal: i64,
    input: NewPlan,
) -> HandlerResult<i64> {
    let amounts = schedule(principal, &input)?;
    let plan = installment_plan::ActiveModel {
        bill_id: Set(bill_id),
        account_id: Set(account_id),
        term_months: Set(input.term_months),
        method: Set(input.method),
        annual_rate_bps: Set(crypto::encrypt_cents(dek, input.annual_rate_bps)),
        fee: Set(crypto::encrypt_cents(dek, input.fee)),
        first_due_date: Set(input.first_due_date),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(err500)?;
    for amount in amounts {
        installment_item::ActiveModel {
            plan_id: Set(plan.id),
            sequence: Set(amount.sequence),
            due_date: Set(amount.due_date),
            principal: Set(crypto::encrypt_cents(dek, amount.principal)),
            interest: Set(crypto::encrypt_cents(dek, amount.interest)),
            fee: Set(crypto::encrypt_cents(dek, amount.fee)),
            total: Set(crypto::encrypt_cents(dek, amount.total)),
            paid_at: Set(None),
            repayment_account_id: Set(None),
            principal_transfer_id: Set(None),
            charge_bill_id: Set(None),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(err500)?;
    }
    Ok(plan.id)
}

fn method_label(method: &str) -> &'static str {
    match method {
        "equal_principal" => "等额本金",
        "flat" => "等本等息",
        _ => "等额本息",
    }
}

fn term_label(months: i32) -> String {
    if months > 24 {
        format!("{} 年（{} 期月供）", months / 12, months)
    } else {
        format!("{months} 期")
    }
}

fn sum_items(
    dek: &crypto::Dek,
    items: &[installment_item::Model],
) -> HandlerResult<(i64, i64, i64)> {
    let mut interest = 0i64;
    let mut fee = 0i64;
    let mut total = 0i64;
    for item in items {
        interest = checked_add(interest, crypto::decrypt_cents(dek, &item.interest))?;
        fee = checked_add(fee, crypto::decrypt_cents(dek, &item.fee))?;
        total = checked_add(total, crypto::decrypt_cents(dek, &item.total))?;
    }
    Ok((interest, fee, total))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<InstallmentsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, query, false).await
}

pub async fn advanced_search(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<InstallmentsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, query, true).await
}

async fn render_list(
    state: &AppState,
    dek: &crypto::Dek,
    mut query: InstallmentsQuery,
    advanced_search: bool,
) -> HandlerResult<Html<String>> {
    if !advanced_search {
        query.mode = "and".into();
        query.status.clear();
        query.method.clear();
        query.start_date.clear();
        query.end_date.clear();
    }
    let start_date = if query.start_date.trim().is_empty() {
        None
    } else {
        Some(
            NaiveDate::parse_from_str(query.start_date.trim(), "%Y-%m-%d")
                .map_err(|_| bad_request("开始日期格式不正确"))?,
        )
    };
    let end_date = if query.end_date.trim().is_empty() {
        None
    } else {
        Some(
            NaiveDate::parse_from_str(query.end_date.trim(), "%Y-%m-%d")
                .map_err(|_| bad_request("结束日期格式不正确"))?,
        )
    };
    if start_date
        .zip(end_date)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(bad_request("开始日期不能晚于结束日期"));
    }
    if !query.status.is_empty()
        && !matches!(
            query.status.as_str(),
            "open" | "completed" | "overdue" | "due_soon"
        )
    {
        query.status.clear();
    }
    if !query.method.is_empty()
        && !matches!(
            query.method.as_str(),
            "equal_payment" | "equal_principal" | "flat"
        )
    {
        query.method.clear();
    }
    let plans = installment_plan::Entity::find()
        .order_by_asc(installment_plan::Column::FirstDueDate)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let bills: HashMap<i64, bill::Model> = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| (bill.id, bill))
        .collect();
    let accounts: HashMap<i64, account::Model> = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|account| (account.id, account))
        .collect();
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let all_items = installment_item::Entity::find()
        .order_by_asc(installment_item::Column::Sequence)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let today = chrono::Local::now().date_naive();
    let soon = today + chrono::Duration::days(7);
    let mut rows = Vec::new();
    for plan in plans {
        let Some(bill) = bills.get(&plan.bill_id) else {
            continue;
        };
        let Some(account) = accounts.get(&plan.account_id) else {
            continue;
        };
        let items = all_items
            .iter()
            .filter(|item| item.plan_id == plan.id)
            .cloned()
            .collect::<Vec<_>>();
        let (_, _, total_repayment) = sum_items(&dek, &items)?;
        let mut remaining = 0i64;
        let mut paid = 0usize;
        let mut overdue_count = 0usize;
        let mut next_due = "已全部还清".to_string();
        let mut next_due_date = None;
        for item in &items {
            if item.paid_at.is_some() {
                paid += 1;
                continue;
            }
            remaining = checked_add(remaining, crypto::decrypt_cents(&dek, &item.total))?;
            if next_due_date.is_none() {
                next_due_date = Some(item.due_date);
                next_due = format!(
                    "{} · {}",
                    item.due_date.format("%Y-%m-%d"),
                    currency::format(crypto::decrypt_cents(&dek, &item.total), &account.currency)
                );
            }
            if item.due_date < today {
                overdue_count += 1;
            }
        }
        let category = crypto::decrypt_string(&dek, &bill.category);
        let note = crypto::decrypt_string(&dek, &bill.note);
        let title = if note.is_empty() {
            category
        } else {
            format!("{category} · {note}")
        };
        rows.push(InstallmentRow {
            id: plan.id,
            title,
            account_name: super::bills::account_display_name(
                &dek,
                account,
                details.get(&account.id),
            ),
            principal: currency::format(
                crypto::decrypt_cents(&dek, &bill.amount),
                &account.currency,
            ),
            term: term_label(plan.term_months),
            method: method_label(&plan.method).into(),
            annual_rate: format!(
                "{}%",
                Decimal::new(crypto::decrypt_cents(&dek, &plan.annual_rate_bps), 2)
            ),
            total_repayment: currency::format(total_repayment, &account.currency),
            remaining: currency::format(remaining, &account.currency),
            next_due,
            progress: format!("{paid}/{}", items.len()),
            overdue_count,
            method_code: plan.method,
            next_due_date,
            completed: paid == items.len(),
        });
    }
    let keyword = query.keyword.trim().to_lowercase();
    let mode_or = query.mode == "or";
    let has_filters = !keyword.is_empty()
        || !query.status.is_empty()
        || !query.method.is_empty()
        || start_date.is_some()
        || end_date.is_some();
    let rows = rows
        .into_iter()
        .filter(|row| {
            let mut conditions = Vec::new();
            if !keyword.is_empty() {
                conditions.push(
                    format!(
                        "{} {} {} {} {}",
                        row.title, row.account_name, row.method, row.term, row.annual_rate
                    )
                    .to_lowercase()
                    .contains(&keyword),
                );
            }
            if !query.status.is_empty() {
                conditions.push(match query.status.as_str() {
                    "completed" => row.completed,
                    "overdue" => row.overdue_count > 0,
                    "due_soon" => row
                        .next_due_date
                        .is_some_and(|date| date >= today && date <= soon),
                    "open" => !row.completed,
                    _ => true,
                });
            }
            if !query.method.is_empty() {
                conditions.push(row.method_code == query.method);
            }
            if start_date.is_some() || end_date.is_some() {
                conditions.push(row.next_due_date.is_some_and(|date| {
                    start_date.is_none_or(|start| date >= start)
                        && end_date.is_none_or(|end| date <= end)
                }));
            }
            if conditions.is_empty() {
                true
            } else if mode_or {
                conditions.into_iter().any(|matched| matched)
            } else {
                conditions.into_iter().all(|matched| matched)
            }
        })
        .collect::<Vec<_>>();
    let total_records = rows.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let rows = rows
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();
    Ok(Html(
        InstallmentsTemplate {
            page_heading: if advanced_search {
                "分期高级搜索"
            } else {
                "分期"
            }
            .into(),
            advanced_search,
            search_action: if advanced_search {
                "/installments/search"
            } else {
                "/installments"
            }
            .into(),
            plans: rows,
            mode: if mode_or { "or" } else { "and" }.into(),
            keyword: query.keyword,
            status: query.status,
            method: query.method,
            start_date: query.start_date,
            end_date: query.end_date,
            has_filters,
            page: pagination.page,
            per_page: pagination.per_page,
            total_pages: pagination.total_pages,
            total_records,
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn detail(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Query(query): Query<InstallmentDetailQuery>,
) -> HandlerResult<Html<String>> {
    let plan = installment_plan::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期计划不存在".into()))?;
    let bill = bill::Entity::find_by_id(plan.bill_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("分期计划关联的账单不存在"))?;
    let account = account::Entity::find_by_id(plan.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("分期计划关联的账户不存在"))?;
    let detail = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let items = installment_item::Entity::find()
        .filter(installment_item::Column::PlanId.eq(id))
        .order_by_asc(installment_item::Column::Sequence)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let repayment_account_models = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|candidate| {
            !matches!(candidate.kind.as_str(), "credit_card" | "credit_service")
                && candidate.currency == account.currency
        })
        .collect::<Vec<_>>();
    let repayment_account_names: HashMap<i64, String> = repayment_account_models
        .iter()
        .map(|candidate| {
            (
                candidate.id,
                super::bills::account_display_name(
                    &dek,
                    candidate,
                    account_details.get(&candidate.id),
                ),
            )
        })
        .collect();
    let repayment_accounts = repayment_account_models
        .into_iter()
        .map(|candidate| RepaymentAccountOption {
            id: candidate.id,
            name: repayment_account_names
                .get(&candidate.id)
                .cloned()
                .unwrap_or_default(),
        })
        .collect();
    let (total_interest, total_fee, total_repayment) = sum_items(&dek, &items)?;
    let today = chrono::Local::now().date_naive();
    let schedule = items
        .into_iter()
        .map(|item| ScheduleRow {
            id: item.id,
            sequence: item.sequence,
            due_date: item.due_date.format("%Y-%m-%d").to_string(),
            principal: currency::format(
                crypto::decrypt_cents(&dek, &item.principal),
                &account.currency,
            ),
            interest: currency::format(
                crypto::decrypt_cents(&dek, &item.interest),
                &account.currency,
            ),
            fee: currency::format(crypto::decrypt_cents(&dek, &item.fee), &account.currency),
            total: currency::format(crypto::decrypt_cents(&dek, &item.total), &account.currency),
            paid: item.paid_at.is_some(),
            overdue: item.paid_at.is_none() && item.due_date < today,
            repayment_account_name: item
                .repayment_account_id
                .and_then(|id| repayment_account_names.get(&id).cloned())
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let total_records = schedule.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let schedule = schedule
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();
    let category = crypto::decrypt_string(&dek, &bill.category);
    let note = crypto::decrypt_string(&dek, &bill.note);
    let title = if note.is_empty() {
        category
    } else {
        format!("{category} · {note}")
    };
    Ok(Html(
        InstallmentDetailTemplate {
            title,
            account_name: super::bills::account_display_name(&dek, &account, detail.as_ref()),
            currency: account.currency.clone(),
            principal: currency::format(
                crypto::decrypt_cents(&dek, &bill.amount),
                &account.currency,
            ),
            term: term_label(plan.term_months),
            method: method_label(&plan.method).into(),
            annual_rate: format!(
                "{}%",
                Decimal::new(crypto::decrypt_cents(&dek, &plan.annual_rate_bps), 2)
            ),
            total_interest: currency::format(total_interest, &account.currency),
            total_fee: currency::format(total_fee, &account.currency),
            total_repayment: currency::format(total_repayment, &account.currency),
            first_due_date: plan.first_due_date.format("%Y-%m-%d").to_string(),
            schedule,
            repayment_accounts,
            plan_id: id,
            page: pagination.page,
            per_page: pagination.per_page,
            total_pages: pagination.total_pages,
            total_records,
        }
        .render()
        .map_err(err500)?,
    ))
}

async fn ensure_fee_category(state: &AppState, dek: &crypto::Dek) -> HandlerResult<()> {
    let exists = category::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .any(|item| {
            item.kind == "expense" && crypto::decrypt_string(dek, &item.name) == "分期费用"
        });
    if !exists {
        category::ActiveModel {
            kind: Set("expense".into()),
            name: Set(crypto::encrypt(dek, "分期费用".as_bytes())),
            is_food: Set(false),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(())
}

pub async fn set_paid(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<PaidFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let item = installment_item::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期期次不存在".into()))?;
    let plan_id = item.plan_id;
    if form.paid {
        if item.paid_at.is_some() {
            return Err(bad_request("该期已经还款，不能重复操作"));
        }
        let repayment_account_id = form
            .repayment_account_id
            .ok_or_else(|| bad_request("请选择还款渠道"))?;
        let plan = installment_plan::Entity::find_by_id(plan_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| err500("分期期次关联的计划不存在"))?;
        let credit_account = account::Entity::find_by_id(plan.account_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| err500("分期计划关联的信用账户不存在"))?;
        let repayment_account = account::Entity::find_by_id(repayment_account_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("还款渠道不存在"))?;
        if matches!(
            repayment_account.kind.as_str(),
            "credit_card" | "credit_service"
        ) {
            return Err(bad_request("信用卡和信贷服务不能作为分期还款渠道"));
        }
        if repayment_account.currency != credit_account.currency {
            return Err(bad_request(
                "分期还款渠道必须与信用账户使用相同货币；如需换汇，请先转入同币种普通账户",
            ));
        }
        let principal = crypto::decrypt_cents(&dek, &item.principal);
        let interest = crypto::decrypt_cents(&dek, &item.interest);
        let fee = crypto::decrypt_cents(&dek, &item.fee);
        let charges = checked_add(interest, fee)?;
        let total = checked_add(principal, charges)?;
        super::accounts::ensure_balance_delta(
            &state,
            &dek,
            repayment_account_id,
            total
                .checked_neg()
                .ok_or_else(|| bad_request("还款金额超出范围"))?,
        )
        .await?;
        if charges > 0 {
            ensure_fee_category(&state, &dek).await?;
        }

        let original_bill = bill::Entity::find_by_id(plan.bill_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| err500("分期计划关联的账单不存在"))?;
        let category_name = crypto::decrypt_string(&dek, &original_bill.category);
        let original_note = crypto::decrypt_string(&dek, &original_bill.note);
        let title = if original_note.is_empty() {
            category_name
        } else {
            format!("{category_name} · {original_note}")
        };
        let happened_at = chrono::Local::now().naive_local();
        let transaction = state.db.begin().await.map_err(err500)?;
        let principal_transfer = transfer::ActiveModel {
            from_account_id: Set(repayment_account_id),
            to_account_id: Set(plan.account_id),
            amount: Set(crypto::encrypt_cents(&dek, principal)),
            to_amount: Set(crypto::encrypt_cents(&dek, principal)),
            note: Set(crypto::encrypt(
                &dek,
                format!("分期还款：{title} · 第 {} 期本金", item.sequence).as_bytes(),
            )),
            happened_at: Set(happened_at),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&transaction)
        .await
        .map_err(err500)?;
        let charge_bill_id = if charges > 0 {
            Some(
                bill::ActiveModel {
                    account_id: Set(repayment_account_id),
                    kind: Set("expense".into()),
                    amount: Set(crypto::encrypt_cents(&dek, charges)),
                    category: Set(crypto::encrypt(&dek, "分期费用".as_bytes())),
                    is_food: Set(false),
                    note: Set(crypto::encrypt(
                        &dek,
                        format!("分期还款：{title} · 第 {} 期利息与手续费", item.sequence)
                            .as_bytes(),
                    )),
                    happened_at: Set(happened_at),
                    created_at: Set(chrono::Utc::now()),
                    ..Default::default()
                }
                .insert(&transaction)
                .await
                .map_err(err500)?
                .id,
            )
        } else {
            None
        };
        let mut active = item.into_active_model();
        active.paid_at = Set(Some(chrono::Utc::now()));
        active.repayment_account_id = Set(Some(repayment_account_id));
        active.principal_transfer_id = Set(Some(principal_transfer.id));
        active.charge_bill_id = Set(charge_bill_id);
        active.update(&transaction).await.map_err(err500)?;
        transaction.commit().await.map_err(err500)?;
    } else {
        if item.paid_at.is_none() {
            return Err(bad_request("该期尚未还款"));
        }
        if let Some(transfer_id) = item.principal_transfer_id {
            if let Some(principal_transfer) = transfer::Entity::find_by_id(transfer_id)
                .one(&state.db)
                .await
                .map_err(err500)?
            {
                super::accounts::ensure_balance_delta(
                    &state,
                    &dek,
                    principal_transfer.to_account_id,
                    super::transfer_to_cents(&dek, &principal_transfer)
                        .checked_neg()
                        .ok_or_else(|| bad_request("还款金额超出范围"))?,
                )
                .await?;
            }
        }
        let transaction = state.db.begin().await.map_err(err500)?;
        if let Some(transfer_id) = item.principal_transfer_id {
            transfer::Entity::delete_by_id(transfer_id)
                .exec(&transaction)
                .await
                .map_err(err500)?;
        }
        if let Some(bill_id) = item.charge_bill_id {
            bill::Entity::delete_by_id(bill_id)
                .exec(&transaction)
                .await
                .map_err(err500)?;
        }
        let mut active = item.into_active_model();
        active.paid_at = Set(None);
        active.repayment_account_id = Set(None);
        active.principal_transfer_id = Set(None);
        active.charge_bill_id = Set(None);
        active.update(&transaction).await.map_err(err500)?;
        transaction.commit().await.map_err(err500)?;
    }
    Ok(Redirect::to(&format!("/installments/{plan_id}")))
}
