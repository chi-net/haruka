use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ActiveModelTrait, EntityTrait, QueryOrder, Set};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto,
    entity::{account, transfer},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const DISPLAY_FMT: &str = "%Y-%m-%d %H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct AccountOption {
    id: i64,
    name: String,
}

struct TransferRow {
    id: i64,
    from_account: String,
    to_account: String,
    amount: String,
    note: String,
    happened_at: String,
}

#[derive(Template)]
#[template(path = "transfers.html")]
struct TransfersTemplate {
    accounts: Vec<AccountOption>,
    transfers: Vec<TransferRow>,
    happened_at: String,
}

#[derive(Deserialize)]
pub struct TransferFormData {
    from_account_id: i64,
    to_account_id: i64,
    amount: String,
    note: String,
    happened_at: String,
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

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let names: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, crypto::decrypt_string(&dek, &account.name)))
        .collect();
    let account_options = accounts
        .into_iter()
        .map(|account| AccountOption {
            id: account.id,
            name: names.get(&account.id).cloned().unwrap_or_default(),
        })
        .collect();
    let rows = transfer::Entity::find()
        .order_by_desc(transfer::Column::HappenedAt)
        .order_by_desc(transfer::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|transfer| TransferRow {
            id: transfer.id,
            from_account: names
                .get(&transfer.from_account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            to_account: names
                .get(&transfer.to_account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            amount: super::fmt_cents(crypto::decrypt_cents(&dek, &transfer.amount)),
            note: crypto::decrypt_string(&dek, &transfer.note),
            happened_at: transfer.happened_at.format(DISPLAY_FMT).to_string(),
        })
        .collect();
    let html = TransfersTemplate {
        accounts: account_options,
        transfers: rows,
        happened_at: chrono::Local::now()
            .naive_local()
            .format(TIME_FMT)
            .to_string(),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<TransferFormData>,
) -> HandlerResult<Redirect> {
    if form.from_account_id == form.to_account_id {
        return Err(bad_request("转出和转入账户不能相同"));
    }
    for account_id in [form.from_account_id, form.to_account_id] {
        if account::Entity::find_by_id(account_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .is_none()
        {
            return Err(bad_request("账户不存在"));
        }
    }
    let amount = parse_amount(&form.amount)?;
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
    Ok(Redirect::to("/transfers"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    transfer::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/transfers"))
}
