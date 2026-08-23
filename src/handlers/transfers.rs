use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::Redirect,
    Form,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde::Deserialize;
use std::str::FromStr;

use crate::{
    crypto,
    entity::{account, transfer},
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
    note: String,
    happened_at: String,
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

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<TransferFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    if form.from_account_id == form.to_account_id {
        return Err(bad_request("转出和转入账户不能相同"));
    }
    let from_account = account::Entity::find_by_id(form.from_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("转出账户不存在"))?;
    if matches!(from_account.kind.as_str(), "credit_card" | "credit_service") {
        return Err(bad_request("信用卡和信贷服务不能作为转账的转出账户"));
    }
    if account::Entity::find_by_id(form.to_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err(bad_request("转入账户不存在"));
    }
    let amount = parse_amount(&form.amount)?;
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        form.from_account_id,
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?,
    )
    .await?;
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    transfer::ActiveModel {
        from_account_id: Set(form.from_account_id),
        to_account_id: Set(form.to_account_id),
        amount: Set(crypto::encrypt_cents(&dek, amount)),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        happened_at: Set(happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/dashboard"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let transfer = transfer::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "转账不存在".into()))?;
    let amount = crypto::decrypt_cents(&dek, &transfer.amount);
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
