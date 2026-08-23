use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, ColumnTrait, Condition, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder, Set,
};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto,
    entity::{account, account_detail, bill, debt_record, transfer},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

const DEFAULT_ACCOUNT_KIND: &str = "other";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

#[derive(Template)]
#[template(path = "accounts.html")]
struct AccountsTemplate {
    accounts: Vec<AccountRow>,
}

struct AccountRow {
    id: i64,
    name: String,
    kind_label: String,
    card_number: String,
    account_username: String,
    note: String,
    balance: String,
}

#[derive(Template)]
#[template(path = "account_form.html")]
struct AccountFormTemplate {
    heading: String,
    action: String,
    name: String,
    account_kind: String,
    card_number: String,
    account_username: String,
    note: String,
}

#[derive(Template)]
#[template(path = "account_balance.html")]
struct AccountBalanceTemplate {
    account_name: String,
    current_balance: String,
    action: String,
}

#[derive(Deserialize)]
pub struct AccountFormData {
    name: String,
    account_kind: String,
    card_number: String,
    account_username: String,
    note: String,
}

#[derive(Deserialize)]
pub struct AccountBalanceFormData {
    balance: String,
}

fn valid_account_kind(kind: &str) -> bool {
    matches!(
        kind,
        "payment" | "bank" | "stored_value" | "investment" | "other"
    )
}

fn account_kind_label(kind: &str) -> &'static str {
    match kind {
        "payment" => "支付账户",
        "bank" => "银行账户",
        "stored_value" => "储值卡账户",
        "investment" => "投资账户",
        _ => "其他",
    }
}

fn mask_card_number(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let mut suffix: Vec<char> = value.chars().rev().take(4).collect();
    suffix.reverse();
    format!("•••• {}", suffix.into_iter().collect::<String>())
}

fn mask_account_username(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 5 {
        if chars.len() == 1 {
            return format!("{}•••", chars[0]);
        }
        return format!("{}•••{}", chars[0], chars[chars.len() - 1]);
    }
    let prefix: String = chars[..3].iter().collect();
    let suffix: String = chars[chars.len() - 2..].iter().collect();
    format!("{prefix}•••{suffix}")
}

async fn save_account_detail(
    state: &AppState,
    dek: &crypto::Dek,
    account_id: i64,
    kind: &str,
    card_number: &str,
    account_username: &str,
) -> HandlerResult<()> {
    let (card_number, account_username) = match kind {
        "payment" => ("", account_username.trim()),
        "bank" | "stored_value" => (card_number.trim(), ""),
        _ => ("", ""),
    };
    if card_number.is_empty() && account_username.is_empty() {
        account_detail::Entity::delete_by_id(account_id)
            .exec(&state.db)
            .await
            .map_err(err500)?;
        return Ok(());
    }

    account_detail::Entity::insert(account_detail::ActiveModel {
        account_id: Set(account_id),
        card_number: Set(crypto::encrypt(dek, card_number.as_bytes())),
        account_username: Set(crypto::encrypt(dek, account_username.as_bytes())),
    })
    .on_conflict(
        OnConflict::column(account_detail::Column::AccountId)
            .update_columns([
                account_detail::Column::CardNumber,
                account_detail::Column::AccountUsername,
            ])
            .to_owned(),
    )
    .exec(&state.db)
    .await
    .map_err(err500)?;
    Ok(())
}

async fn transaction_balance(
    state: &AppState,
    dek: &crypto::Dek,
    account_id: i64,
) -> HandlerResult<i64> {
    let bills = bill::Entity::find()
        .filter(bill::Column::AccountId.eq(account_id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut total = 0i64;
    for bill in bills {
        let cents = crypto::decrypt_cents(dek, &bill.amount);
        let signed = if bill.kind == "income" {
            cents
        } else {
            cents.checked_neg().ok_or_else(|| err500("余额超出范围"))?
        };
        total = total
            .checked_add(signed)
            .ok_or_else(|| err500("余额超出范围"))?;
    }
    let transfers = transfer::Entity::find()
        .filter(
            Condition::any()
                .add(transfer::Column::FromAccountId.eq(account_id))
                .add(transfer::Column::ToAccountId.eq(account_id)),
        )
        .all(&state.db)
        .await
        .map_err(err500)?;
    for transfer in transfers {
        let cents = crypto::decrypt_cents(dek, &transfer.amount);
        let signed = if transfer.to_account_id == account_id {
            cents
        } else {
            cents.checked_neg().ok_or_else(|| err500("余额超出范围"))?
        };
        total = total
            .checked_add(signed)
            .ok_or_else(|| err500("余额超出范围"))?;
    }
    let debt_records = debt_record::Entity::find()
        .filter(debt_record::Column::AccountId.eq(account_id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    for record in debt_records {
        let cents = crypto::decrypt_cents(dek, &record.amount);
        let signed = if matches!(record.kind.as_str(), "borrow" | "repayment_received") {
            cents
        } else {
            cents.checked_neg().ok_or_else(|| err500("余额超出范围"))?
        };
        total = total
            .checked_add(signed)
            .ok_or_else(|| err500("余额超出范围"))?;
    }
    Ok(total)
}

async fn current_balance(
    state: &AppState,
    dek: &crypto::Dek,
    account_id: i64,
) -> HandlerResult<i64> {
    let account = account::Entity::find_by_id(account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    transaction_balance(state, dek, account_id)
        .await?
        .checked_add(crypto::decrypt_cents(dek, &account.balance_offset))
        .ok_or_else(|| err500("余额超出范围"))
}

fn parse_balance(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("余额格式不正确"))?
        .round_dp(2);
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("余额超出范围"))
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
    let bills = bill::Entity::find().all(&state.db).await.map_err(err500)?;
    let transfers = transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let debt_records = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let details: HashMap<i64, (String, String)> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| {
            let card_number = crypto::decrypt_string(&dek, &detail.card_number);
            let account_username = crypto::decrypt_string(&dek, &detail.account_username);
            (
                detail.account_id,
                (
                    mask_card_number(&card_number),
                    mask_account_username(&account_username),
                ),
            )
        })
        .collect();
    let mut net: HashMap<i64, i64> = accounts
        .iter()
        .map(|account| {
            (
                account.id,
                crypto::decrypt_cents(&dek, &account.balance_offset),
            )
        })
        .collect();

    for bill in &bills {
        let cents = crypto::decrypt_cents(&dek, &bill.amount);
        let signed = if bill.kind == "income" {
            cents
        } else {
            cents.checked_neg().ok_or_else(|| err500("余额超出范围"))?
        };
        let current = net.get(&bill.account_id).copied().unwrap_or_default();
        net.insert(
            bill.account_id,
            current
                .checked_add(signed)
                .ok_or_else(|| err500("余额超出范围"))?,
        );
    }
    for transfer in transfers {
        let cents = crypto::decrypt_cents(&dek, &transfer.amount);
        let outgoing = cents.checked_neg().ok_or_else(|| err500("余额超出范围"))?;
        for (account_id, signed) in [
            (transfer.from_account_id, outgoing),
            (transfer.to_account_id, cents),
        ] {
            let current = net.get(&account_id).copied().unwrap_or_default();
            net.insert(
                account_id,
                current
                    .checked_add(signed)
                    .ok_or_else(|| err500("余额超出范围"))?,
            );
        }
    }
    for record in debt_records {
        let cents = crypto::decrypt_cents(&dek, &record.amount);
        let signed = if matches!(record.kind.as_str(), "borrow" | "repayment_received") {
            cents
        } else {
            cents.checked_neg().ok_or_else(|| err500("余额超出范围"))?
        };
        let current = net.get(&record.account_id).copied().unwrap_or_default();
        net.insert(
            record.account_id,
            current
                .checked_add(signed)
                .ok_or_else(|| err500("余额超出范围"))?,
        );
    }

    let rows = accounts
        .into_iter()
        .map(|account| {
            let (card_number, account_username) =
                details.get(&account.id).cloned().unwrap_or_default();
            AccountRow {
                id: account.id,
                name: crypto::decrypt_string(&dek, &account.name),
                kind_label: account_kind_label(&account.kind).into(),
                card_number,
                account_username,
                note: crypto::decrypt_string(&dek, &account.note),
                balance: super::fmt_cents(net.get(&account.id).copied().unwrap_or_default()),
            }
        })
        .collect();

    let html = AccountsTemplate { accounts: rows }
        .render()
        .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_form() -> Html<String> {
    let html = AccountFormTemplate {
        heading: "新建账户".into(),
        action: "/accounts".into(),
        name: String::new(),
        account_kind: DEFAULT_ACCOUNT_KIND.into(),
        card_number: String::new(),
        account_username: String::new(),
        note: String::new(),
    }
    .render()
    .expect("模板渲染失败");
    Html(html)
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<AccountFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad_request("账户名不能为空"));
    }
    if !valid_account_kind(&form.account_kind) {
        return Err(bad_request("账户类型无效"));
    }
    let account = account::ActiveModel {
        name: Set(crypto::encrypt(&dek, form.name.trim().as_bytes())),
        kind: Set(form.account_kind.clone()),
        balance_offset: Set(crypto::encrypt_cents(&dek, 0)),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    save_account_detail(
        &state,
        &dek,
        account.id,
        &form.account_kind,
        &form.card_number,
        &form.account_username,
    )
    .await?;
    Ok(Redirect::to("/accounts"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let detail = account_detail::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let card_number = detail
        .as_ref()
        .map(|detail| crypto::decrypt_string(&dek, &detail.card_number))
        .unwrap_or_default();
    let account_username = detail
        .as_ref()
        .map(|detail| crypto::decrypt_string(&dek, &detail.account_username))
        .unwrap_or_default();
    let html = AccountFormTemplate {
        heading: "编辑账户".into(),
        action: format!("/accounts/{id}/edit"),
        name: crypto::decrypt_string(&dek, &account.name),
        account_kind: account.kind,
        card_number,
        account_username,
        note: crypto::decrypt_string(&dek, &account.note),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<AccountFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad_request("账户名不能为空"));
    }
    if !valid_account_kind(&form.account_kind) {
        return Err(bad_request("账户类型无效"));
    }
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let mut active = account.into_active_model();
    active.name = Set(crypto::encrypt(&dek, form.name.trim().as_bytes()));
    active.kind = Set(form.account_kind.clone());
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    save_account_detail(
        &state,
        &dek,
        id,
        &form.account_kind,
        &form.card_number,
        &form.account_username,
    )
    .await?;
    Ok(Redirect::to("/accounts"))
}

pub async fn balance_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let html = AccountBalanceTemplate {
        account_name: crypto::decrypt_string(&dek, &account.name),
        current_balance: super::fmt_cents(current_balance(&state, &dek, id).await?),
        action: format!("/accounts/{id}/balance"),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn force_balance(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<AccountBalanceFormData>,
) -> HandlerResult<Redirect> {
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let target = parse_balance(&form.balance)?;
    let offset = target
        .checked_sub(transaction_balance(&state, &dek, id).await?)
        .ok_or_else(|| bad_request("余额超出范围"))?;
    let mut active = account.into_active_model();
    active.balance_offset = Set(crypto::encrypt_cents(&dek, offset));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/accounts"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    account::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/accounts"))
}
