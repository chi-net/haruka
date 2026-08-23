use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::{Months, NaiveDate};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder, Set,
};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{account, account_detail, bill, installment_item, installment_plan},
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
}

#[derive(Template)]
#[template(path = "installments.html")]
struct InstallmentsTemplate {
    plans: Vec<InstallmentRow>,
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
}

#[derive(Deserialize)]
pub struct PaidFormData {
    paid: bool,
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
) -> HandlerResult<Html<String>> {
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
        let mut next_due = "已全部标记还清".to_string();
        for item in &items {
            if item.paid_at.is_some() {
                paid += 1;
                continue;
            }
            remaining = checked_add(remaining, crypto::decrypt_cents(&dek, &item.total))?;
            if next_due == "已全部标记还清" {
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
        });
    }
    Ok(Html(
        InstallmentsTemplate { plans: rows }
            .render()
            .map_err(err500)?,
    ))
}

pub async fn detail(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
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
        })
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
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn set_paid(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<PaidFormData>,
) -> HandlerResult<Redirect> {
    let item = installment_item::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期期次不存在".into()))?;
    let plan_id = item.plan_id;
    let mut active = item.into_active_model();
    active.paid_at = Set(form.paid.then(chrono::Utc::now));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to(&format!("/installments/{plan_id}")))
}
