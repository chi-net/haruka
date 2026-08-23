use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto,
    entity::{account, account_detail, bill, category},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct BillRow {
    id: i64,
    account_name: String,
    kind: String,
    amount: String,
    category: String,
    note: String,
    happened_at: String,
}

struct AccountOption {
    id: i64,
    name: String,
}

struct CategoryOption {
    kind: String,
    name: String,
}

#[derive(Template)]
#[template(path = "bills.html")]
struct BillsTemplate {
    bills: Vec<BillRow>,
    total_income: String,
    total_expense: String,
    net: String,
}

#[derive(Template)]
#[template(path = "bill_form.html")]
struct BillFormTemplate {
    heading: String,
    action: String,
    accounts: Vec<AccountOption>,
    categories: Vec<CategoryOption>,
    account_id: i64,
    kind: String,
    amount: String,
    category: String,
    note: String,
    happened_at: String,
}

#[derive(Deserialize)]
pub struct BillFormData {
    account_id: String,
    kind: String,
    amount: String,
    category: String,
    note: String,
    happened_at: String,
}

struct ParsedBill {
    account_id: i64,
    kind: String,
    amount: i64,
    category: String,
    note: String,
    happened_at: NaiveDateTime,
}

fn signed_amount(kind: &str, amount: i64) -> HandlerResult<i64> {
    if kind == "income" {
        Ok(amount)
    } else {
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))
    }
}

const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const DISPLAY_FMT: &str = "%Y-%m-%d %H:%M";

fn parse_form(form: BillFormData) -> HandlerResult<ParsedBill> {
    let account_id: i64 = form
        .account_id
        .parse()
        .map_err(|_| bad_request("账户无效"))?;
    if form.kind != "income" && form.kind != "expense" {
        return Err(bad_request("类型必须是收入或支出"));
    }
    let amount_dec = Decimal::from_str(form.amount.trim())
        .map_err(|_| bad_request("金额格式不正确"))?
        .round_dp(2);
    if amount_dec <= Decimal::ZERO {
        return Err(bad_request("金额必须大于 0"));
    }
    let amount = (amount_dec * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("金额超出范围"))?;
    if form.category.trim().is_empty() {
        return Err(bad_request("分类不能为空"));
    }
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    Ok(ParsedBill {
        account_id,
        kind: form.kind,
        amount,
        category: form.category.trim().to_string(),
        note: form.note.trim().to_string(),
        happened_at,
    })
}

async fn account_options(state: &AppState, dek: &crypto::Dek) -> HandlerResult<Vec<AccountOption>> {
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    if accounts.is_empty() {
        return Err(bad_request("请先创建账户"));
    }
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    Ok(accounts
        .into_iter()
        .map(|a| AccountOption {
            id: a.id,
            name: account_display_name(dek, &a, details.get(&a.id)),
        })
        .collect())
}

pub(crate) fn account_display_name(
    dek: &crypto::Dek,
    account: &account::Model,
    detail: Option<&account_detail::Model>,
) -> String {
    let name = crypto::decrypt_string(dek, &account.name);
    let Some(detail) = detail else {
        return name;
    };
    let card_number = crypto::decrypt_string(dek, &detail.card_number);
    if !card_number.is_empty() {
        return format!("{name} · 卡号 {}", super::mask_card_number(&card_number));
    }
    let username = crypto::decrypt_string(dek, &detail.account_username);
    if !username.is_empty() {
        return format!(
            "{name} · 用户名 {}",
            super::mask_account_username(&username)
        );
    }
    name
}

async fn category_options(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<Vec<CategoryOption>> {
    Ok(category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|category| CategoryOption {
            kind: category.kind,
            name: crypto::decrypt_string(dek, &category.name),
        })
        .collect())
}

pub(crate) async fn ensure_category_exists(
    state: &AppState,
    dek: &crypto::Dek,
    kind: &str,
    name: &str,
) -> HandlerResult<()> {
    let exists = category_options(state, dek)
        .await?
        .into_iter()
        .any(|category| category.kind == kind && category.name == name);
    if !exists {
        return Err(bad_request("请选择设置中已有的对应收支分类"));
    }
    Ok(())
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let bills = bill::Entity::find()
        .order_by_desc(bill::Column::HappenedAt)
        .order_by_desc(bill::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let accounts = account::Entity::find()
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
    let names: HashMap<i64, String> = accounts
        .into_iter()
        .map(|a| (a.id, account_display_name(&dek, &a, details.get(&a.id))))
        .collect();

    let mut total_income: i64 = 0;
    let mut total_expense: i64 = 0;
    let mut rows = Vec::with_capacity(bills.len());
    for b in bills {
        let cents = crypto::decrypt_cents(&dek, &b.amount);
        if b.kind == "income" {
            total_income += cents;
        } else {
            total_expense += cents;
        }
        rows.push(BillRow {
            id: b.id,
            account_name: names
                .get(&b.account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            kind: b.kind,
            amount: super::fmt_cents(cents),
            category: crypto::decrypt_string(&dek, &b.category),
            note: crypto::decrypt_string(&dek, &b.note),
            happened_at: b.happened_at.format(DISPLAY_FMT).to_string(),
        });
    }

    let html = BillsTemplate {
        bills: rows,
        total_income: super::fmt_cents(total_income),
        total_expense: super::fmt_cents(total_expense),
        net: super::fmt_cents(total_income - total_expense),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let accounts = account_options(&state, &dek).await?;
    let categories = category_options(&state, &dek).await?;
    let first_id = accounts[0].id;
    let html = BillFormTemplate {
        heading: "记一笔".into(),
        action: "/bills".into(),
        accounts,
        categories,
        account_id: first_id,
        kind: "expense".into(),
        amount: String::new(),
        category: String::new(),
        note: String::new(),
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
    Form(form): Form<BillFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let parsed = parse_form(form)?;
    ensure_category_exists(&state, &dek, &parsed.kind, &parsed.category).await?;
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        parsed.account_id,
        signed_amount(&parsed.kind, parsed.amount)?,
    )
    .await?;
    bill::ActiveModel {
        account_id: Set(parsed.account_id),
        kind: Set(parsed.kind),
        amount: Set(crypto::encrypt_cents(&dek, parsed.amount)),
        category: Set(crypto::encrypt(&dek, parsed.category.as_bytes())),
        note: Set(crypto::encrypt(&dek, parsed.note.as_bytes())),
        happened_at: Set(parsed.happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/bills"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let b = bill::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    let accounts = account_options(&state, &dek).await?;
    let categories = category_options(&state, &dek).await?;
    let html = BillFormTemplate {
        heading: "编辑账单".into(),
        action: format!("/bills/{id}/edit"),
        accounts,
        categories,
        account_id: b.account_id,
        kind: b.kind,
        amount: super::fmt_cents(crypto::decrypt_cents(&dek, &b.amount)),
        category: crypto::decrypt_string(&dek, &b.category),
        note: crypto::decrypt_string(&dek, &b.note),
        happened_at: b.happened_at.format(TIME_FMT).to_string(),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<BillFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let parsed = parse_form(form)?;
    ensure_category_exists(&state, &dek, &parsed.kind, &parsed.category).await?;
    let b = bill::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    let old_amount = crypto::decrypt_cents(&dek, &b.amount);
    let old_signed = signed_amount(&b.kind, old_amount)?;
    let new_signed = signed_amount(&parsed.kind, parsed.amount)?;
    if b.account_id == parsed.account_id {
        let delta = new_signed
            .checked_sub(old_signed)
            .ok_or_else(|| bad_request("金额超出范围"))?;
        super::accounts::ensure_balance_delta(&state, &dek, b.account_id, delta).await?;
    } else {
        super::accounts::ensure_balance_delta(
            &state,
            &dek,
            b.account_id,
            old_signed
                .checked_neg()
                .ok_or_else(|| bad_request("金额超出范围"))?,
        )
        .await?;
        super::accounts::ensure_balance_delta(&state, &dek, parsed.account_id, new_signed).await?;
    }
    let mut active = b.into_active_model();
    active.account_id = Set(parsed.account_id);
    active.kind = Set(parsed.kind);
    active.amount = Set(crypto::encrypt_cents(&dek, parsed.amount));
    active.category = Set(crypto::encrypt(&dek, parsed.category.as_bytes()));
    active.note = Set(crypto::encrypt(&dek, parsed.note.as_bytes()));
    active.happened_at = Set(parsed.happened_at);
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/bills"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let b = bill::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    let signed = signed_amount(&b.kind, crypto::decrypt_cents(&dek, &b.amount))?;
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        b.account_id,
        signed
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?,
    )
    .await?;
    bill::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/bills"))
}
