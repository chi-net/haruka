use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set,
};
use serde::Deserialize;
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

#[derive(Template)]
#[template(path = "accounts.html")]
struct AccountsTemplate {
    accounts: Vec<AccountRow>,
}

struct AccountRow {
    id: i64,
    name: String,
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
    card_number: String,
    account_username: String,
    note: String,
}

#[derive(Deserialize)]
pub struct AccountFormData {
    name: String,
    card_number: String,
    account_username: String,
    note: String,
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
    card_number: &str,
    account_username: &str,
) -> HandlerResult<()> {
    let card_number = card_number.trim();
    let account_username = account_username.trim();
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

    let mut net: HashMap<i64, i64> = HashMap::new();
    for b in &bills {
        let cents = crypto::decrypt_cents(&dek, &b.amount);
        let sign = if b.kind == "income" { cents } else { -cents };
        *net.entry(b.account_id).or_default() += sign;
    }

    let rows = accounts
        .into_iter()
        .map(|a| {
            let (card_number, account_username) = details.get(&a.id).cloned().unwrap_or_default();
            AccountRow {
                id: a.id,
                name: crypto::decrypt_string(&dek, &a.name),
                card_number,
                account_username,
                note: crypto::decrypt_string(&dek, &a.note),
                balance: super::fmt_cents(net.get(&a.id).copied().unwrap_or_default()),
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
        return Err((StatusCode::BAD_REQUEST, "账户名不能为空".into()));
    }
    let account = account::ActiveModel {
        name: Set(crypto::encrypt(&dek, form.name.trim().as_bytes())),
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
    let a = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let detail = account_detail::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let html = AccountFormTemplate {
        heading: "编辑账户".into(),
        action: format!("/accounts/{id}/edit"),
        name: crypto::decrypt_string(&dek, &a.name),
        card_number: detail
            .as_ref()
            .map(|detail| crypto::decrypt_string(&dek, &detail.card_number))
            .unwrap_or_default(),
        account_username: detail
            .as_ref()
            .map(|detail| crypto::decrypt_string(&dek, &detail.account_username))
            .unwrap_or_default(),
        note: crypto::decrypt_string(&dek, &a.note),
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
        return Err((StatusCode::BAD_REQUEST, "账户名不能为空".into()));
    }
    let a = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let mut active = a.into_active_model();
    active.name = Set(crypto::encrypt(&dek, form.name.trim().as_bytes()));
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    save_account_detail(&state, &dek, id, &form.card_number, &form.account_username).await?;
    Ok(Redirect::to("/accounts"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    account::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/accounts"))
}
