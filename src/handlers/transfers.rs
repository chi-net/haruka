use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Form, Json,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::{
    crypto, currency,
    entity::{account, installment_item, investment_execution, transfer},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

#[derive(Deserialize)]
pub struct TransferFormData {
    from_account_id: i64,
    to_account_id: i64,
    amount: String,
    #[serde(default)]
    to_amount: String,
    note: String,
    happened_at: String,
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
pub struct QuoteQuery {
    from_account_id: i64,
    to_account_id: i64,
    amount: String,
    happened_at: String,
}

#[derive(Serialize)]
pub struct QuoteResponse {
    from_currency: String,
    to_currency: String,
    to_amount: String,
}

#[derive(Serialize)]
pub struct TransferCreateResponse {
    ok: bool,
    message: String,
    redirect: String,
}

#[derive(Deserialize)]
pub struct DeleteFormData {
    #[serde(default)]
    redirect_to: Option<String>,
}

fn parse_amount(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("金额格式不正确"))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request("金额必须大于 0"));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("金额超出范围"))
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
}

pub async fn quote(
    State(state): State<AppState>,
    Query(query): Query<QuoteQuery>,
) -> HandlerResult<Json<QuoteResponse>> {
    if query.from_account_id == query.to_account_id {
        return Err(bad_request("转出和转入账户不能相同"));
    }
    let from_account = account::Entity::find_by_id(query.from_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("转出账户不存在"))?;
    let to_account = account::Entity::find_by_id(query.to_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("转入账户不存在"))?;
    let amount = parse_amount(&query.amount)?;
    NaiveDateTime::parse_from_str(query.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    let to_amount = currency::convert_cents(
        &state,
        amount,
        &from_account.currency,
        &to_account.currency,
        chrono::Local::now().date_naive(),
    )
    .await
    .map_err(err500)?;
    Ok(Json(QuoteResponse {
        from_currency: from_account.currency,
        to_currency: to_account.currency,
        to_amount: super::fmt_cents(to_amount),
    }))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    headers: HeaderMap,
    Form(form): Form<TransferFormData>,
) -> HandlerResult<Response> {
    let _balance_guard = state.balance_writes.lock().await;
    if form.from_account_id == form.to_account_id {
        return Err(bad_request("转出和转入账户不能相同"));
    }
    let from_account = account::Entity::find_by_id(form.from_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("转出账户不存在"))?;
    let to_account = account::Entity::find_by_id(form.to_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("转入账户不存在"))?;
    let amount = parse_amount(&form.amount)?;
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    let to_amount = if from_account.currency == to_account.currency {
        amount
    } else if form.to_amount.trim().is_empty() {
        currency::convert_cents(
            &state,
            amount,
            &from_account.currency,
            &to_account.currency,
            chrono::Local::now().date_naive(),
        )
        .await
        .map_err(err500)?
    } else {
        parse_amount(&form.to_amount)?
    };
    if matches!(from_account.kind.as_str(), "credit_card" | "credit_service") {
        let balance = super::accounts::current_balance(&state, &dek, from_account.id).await?;
        if balance <= 0 {
            return Err(bad_request("信用账户只有在余额为正时才允许转出"));
        }
        if amount > balance {
            return Err(bad_request("信用账户转出后余额不能低于 0"));
        }
    } else {
        super::accounts::ensure_balance_delta(
            &state,
            &dek,
            form.from_account_id,
            amount
                .checked_neg()
                .ok_or_else(|| bad_request("金额超出范围"))?,
        )
        .await?;
    }
    super::accounts::ensure_balance_delta(&state, &dek, form.to_account_id, to_amount).await?;
    transfer::ActiveModel {
        from_account_id: Set(form.from_account_id),
        to_account_id: Set(form.to_account_id),
        amount: Set(crypto::encrypt_cents(&dek, amount)),
        to_amount: Set(crypto::encrypt_cents(&dek, to_amount)),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        happened_at: Set(happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    let redirect = if form.redirect_to.as_deref() == Some("/bills") {
        "/bills"
    } else {
        "/dashboard"
    };
    if accepts_json(&headers) {
        Ok(Json(TransferCreateResponse {
            ok: true,
            message: "转账已记录".into(),
            redirect: redirect.into(),
        })
        .into_response())
    } else {
        Ok(Redirect::to(redirect).into_response())
    }
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    if installment_item::Entity::find()
        .filter(installment_item::Column::PrincipalTransferId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "分期还款本金流水不能单独删除，请在分期详情中撤销对应还款",
        ));
    }
    if investment_execution::Entity::find()
        .filter(investment_execution::Column::TransferId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "定投生成的转账不能单独删除；删除定投计划不会影响已有流水",
        ));
    }
    let transfer = transfer::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "转账不存在".into()))?;
    let from_amount = crypto::decrypt_cents(&dek, &transfer.amount);
    let amount = super::transfer_to_cents(&dek, &transfer);
    super::accounts::ensure_balance_delta(&state, &dek, transfer.from_account_id, from_amount)
        .await?;
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        transfer.to_account_id,
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?,
    )
    .await?;
    transfer::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to(
        if form.redirect_to.as_deref() == Some("/bills") {
            "/bills"
        } else {
            "/dashboard"
        },
    ))
}
