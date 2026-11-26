use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Extension, State},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, Set, TransactionTrait,
};
use serde::Deserialize;

use crate::{
    credit_dates::{installment_billing_close, next_billing_close},
    credit_statements::{self, Ledger},
    crypto,
    entity::{
        account, account_detail, balance_adjustment, bill, debt_record, installment_item,
        installment_plan, meta, transfer,
    },
    installment_state::RefundContext,
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (axum::http::StatusCode, String)>;
fn err500(error: impl std::fmt::Display) -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        error.to_string(),
    )
}
fn bad_request(message: &str) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::BAD_REQUEST, message.into())
}

struct ProposedItem {
    item_id: i64,
    plan: String,
    due_date: String,
    total: String,
    principal: String,
}

struct ReconcileCandidate {
    account_name: String,
    currency: String,
    before_due: String,
    before_remaining: String,
    before_unbilled: String,
    after_due: String,
    after_remaining: String,
    items: Vec<ProposedItem>,
}

#[derive(Template)]
#[template(path = "installment_reconcile.html")]
struct ReconcileTemplate {
    candidates: Vec<ReconcileCandidate>,
}

#[derive(Deserialize)]
pub struct ReconcileForm {
    #[serde(default)]
    confirm: bool,
}

async fn candidates(
    state: &AppState,
    dek: &crypto::Dek,
    time_zone: super::ClientTimeZone,
) -> HandlerResult<Vec<ReconcileCandidate>> {
    let bills = bill::Entity::find().all(&state.db).await.map_err(err500)?;
    let transfers = transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let debts = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let adjustments = balance_adjustment::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let plans: HashMap<i64, installment_plan::Model> = installment_plan::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|plan| (plan.id, plan))
        .collect();
    let items = installment_item::Entity::find()
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
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let refunds = RefundContext::new(dek, &bills).map_err(|error| bad_request(&error))?;
    let today = time_zone.today();
    let mut out = Vec::new();
    for account in accounts
        .iter()
        .filter(|account| matches!(account.kind.as_str(), "credit_card" | "credit_service"))
    {
        let detail = details.get(&account.id);
        let balance = super::accounts::current_balance(state, dek, account.id).await?;
        let ledger = Ledger {
            bills: &bills,
            transfers: &transfers,
            debts: &debts,
            adjustments: &adjustments,
            plans: &plans,
            items: &items,
        };
        let statement =
            credit_statements::calculate(account, detail, &ledger, dek, time_zone, today)
                .map_err(|error| bad_request(&error))?;
        let before_due = statement
            .due
            .iter()
            .try_fold(0i64, |total, due| total.checked_add(due.total))
            .ok_or_else(|| bad_request("金额超出范围"))?;
        let before_remaining = statement.installment_principal;
        let before_unbilled = statement
            .unbilled
            .checked_add(statement.unbilled_installment)
            .ok_or_else(|| bad_request("金额超出范围"))?;
        let debt = balance.checked_neg().unwrap_or(0).max(0);
        let next_close = next_billing_close(today, detail).map_err(|error| bad_request(&error))?;
        let mut proposed = Vec::new();
        let mut due_installment_total = 0i64;
        for plan in plans
            .values()
            .filter(|plan| plan.account_id == account.id && plan.deleted_at.is_none())
        {
            let refund = refunds
                .state(dek, plan)
                .map_err(|error| bad_request(&error))?;
            if refund.full_refund || refund.needs_review {
                continue;
            }
            for item in items
                .iter()
                .filter(|item| item.plan_id == plan.id && item.paid_at.is_none())
            {
                let close = installment_billing_close(item.due_date, detail)
                    .map_err(|error| bad_request(&error))?;
                let include = close <= today || close == next_close;
                if include {
                    if close <= today {
                        due_installment_total = due_installment_total
                            .checked_add(crypto::decrypt_cents(dek, &item.total))
                            .ok_or_else(|| bad_request("金额超出范围"))?;
                    }
                    proposed.push(item.clone());
                }
            }
        }
        if proposed.is_empty() {
            continue;
        }
        // Only offer migration where the old schedule is visibly inconsistent with the balance.
        if !(due_installment_total > 0 && before_due > debt || (balance > 0 && before_unbilled > 0))
        {
            continue;
        }
        let principal = proposed
            .iter()
            .try_fold(0i64, |total, item| {
                total.checked_add(crypto::decrypt_cents(dek, &item.principal))
            })
            .ok_or_else(|| bad_request("金额超出范围"))?;
        let rows = proposed
            .iter()
            .map(|item| ProposedItem {
                item_id: item.id,
                plan: format!("分期计划 #{}", item.plan_id),
                due_date: item.due_date.format("%Y-%m-%d").to_string(),
                total: super::fmt_cents(crypto::decrypt_cents(dek, &item.total)),
                principal: super::fmt_cents(crypto::decrypt_cents(dek, &item.principal)),
            })
            .collect();
        out.push(ReconcileCandidate {
            account_name: crypto::decrypt_string(dek, &account.name),
            currency: account.currency.clone(),
            before_due: super::fmt_cents(before_due),
            before_remaining: super::fmt_cents(before_remaining),
            before_unbilled: super::fmt_cents(before_unbilled),
            after_due: super::fmt_cents(before_due.saturating_sub(due_installment_total)),
            after_remaining: super::fmt_cents(before_remaining.saturating_sub(principal)),
            items: rows,
        });
    }
    Ok(out)
}

pub async fn preview(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
) -> HandlerResult<Html<String>> {
    let candidates = candidates(&state, &dek, time_zone).await?;
    Ok(Html(
        ReconcileTemplate { candidates }.render().map_err(err500)?,
    ))
}

pub async fn apply(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Form(form): Form<ReconcileForm>,
) -> HandlerResult<Response> {
    if !form.confirm {
        return Err(bad_request("必须确认预览中的变更"));
    }
    let _guard = state.balance_writes.lock().await;
    let found = candidates(&state, &dek, time_zone).await?;
    let tx = state.db.begin().await.map_err(err500)?;
    let now = Utc::now();
    let mut changed = 0usize;
    for candidate in found {
        for item in candidate.items {
            let updated = installment_item::Entity::update_many()
                .col_expr(
                    installment_item::Column::PaidAt,
                    sea_orm::sea_query::Expr::value(now),
                )
                .filter(installment_item::Column::Id.eq(item.item_id))
                .filter(installment_item::Column::PaidAt.is_null())
                .exec(&tx)
                .await
                .map_err(err500)?;
            changed += updated.rows_affected as usize;
        }
    }
    if let Some(meta) = meta::Entity::find_by_id(1).one(&tx).await.map_err(err500)? {
        let mut active = meta.into_active_model();
        active.installment_reconcile_state = Set("done".into());
        active.update(&tx).await.map_err(err500)?;
    }
    tx.commit().await.map_err(err500)?;
    drop(_guard);
    super::installments::repair_paid_installment_bills(&state, &dek).await?;
    let _ = changed;
    Ok(Redirect::to("/installments/reconcile").into_response())
}

pub async fn decline(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
) -> HandlerResult<Response> {
    if let Some(meta) = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        let mut active = meta.into_active_model();
        active.installment_reconcile_state = Set("declined".into());
        active.update(&state.db).await.map_err(err500)?;
    }
    Ok(Redirect::to("/dashboard").into_response())
}
