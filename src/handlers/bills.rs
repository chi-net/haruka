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
    entity::{account, account_detail, bill, category, debt_person, debt_record, transfer},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

pub(crate) struct LedgerRow {
    pub(crate) happened_at: String,
    pub(crate) record_type: String,
    pub(crate) account_name: String,
    pub(crate) subject: String,
    pub(crate) note: String,
    pub(crate) amount: String,
    pub(crate) money_class: String,
    pub(crate) edit_url: String,
    pub(crate) delete_action: String,
    pub(crate) delete_confirm: String,
    pub(crate) sort_key: NaiveDateTime,
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
    records: Vec<LedgerRow>,
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
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteFormData {
    #[serde(default)]
    redirect_to: Option<String>,
}

fn ledger_redirect(redirect_to: Option<&str>) -> &'static str {
    if redirect_to == Some("/dashboard") {
        "/dashboard"
    } else {
        "/bills"
    }
}

struct ParsedBill {
    account_id: i64,
    kind: String,
    amount: i64,
    category: String,
    note: String,
    happened_at: NaiveDateTime,
    redirect_to: String,
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
        redirect_to: if form.redirect_to.as_deref() == Some("/dashboard") {
            "/dashboard".into()
        } else {
            "/bills".into()
        },
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

fn debt_kind_label(kind: &str) -> &'static str {
    match kind {
        "lend" => "借出",
        "borrow" => "借入",
        "repayment_received" => "收回还款",
        "repayment_paid" => "归还借款",
        _ => "借还",
    }
}

pub(crate) async fn ledger_rows(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<Vec<LedgerRow>> {
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
    let account_names: HashMap<i64, String> = accounts
        .into_iter()
        .map(|account| {
            (
                account.id,
                account_display_name(dek, &account, details.get(&account.id)),
            )
        })
        .collect();
    let person_names: HashMap<i64, String> = debt_person::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|person| (person.id, crypto::decrypt_string(dek, &person.name)))
        .collect();

    let mut rows = Vec::new();
    for bill in bill::Entity::find().all(&state.db).await.map_err(err500)? {
        let incoming = bill.kind == "income";
        let amount = crypto::decrypt_cents(dek, &bill.amount);
        rows.push(LedgerRow {
            happened_at: bill.happened_at.format(DISPLAY_FMT).to_string(),
            record_type: if incoming { "收入" } else { "支出" }.into(),
            account_name: account_names
                .get(&bill.account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            subject: crypto::decrypt_string(dek, &bill.category),
            note: crypto::decrypt_string(dek, &bill.note),
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
            edit_url: format!("/bills/{}/edit", bill.id),
            delete_action: format!("/bills/{}/delete", bill.id),
            delete_confirm: "确认删除这条账单？".into(),
            sort_key: bill.happened_at,
        });
    }

    for transfer in transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        rows.push(LedgerRow {
            happened_at: transfer.happened_at.format(DISPLAY_FMT).to_string(),
            record_type: "转账".into(),
            account_name: format!(
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
            subject: "账户间".into(),
            note: crypto::decrypt_string(dek, &transfer.note),
            amount: super::fmt_cents(crypto::decrypt_cents(dek, &transfer.amount)),
            money_class: "text-blue-600".into(),
            edit_url: String::new(),
            delete_action: format!("/transfers/{}/delete", transfer.id),
            delete_confirm: "确认删除这条转账？".into(),
            sort_key: transfer.happened_at,
        });
    }

    for record in debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let incoming = matches!(record.kind.as_str(), "borrow" | "repayment_received");
        let amount = crypto::decrypt_cents(dek, &record.amount);
        rows.push(LedgerRow {
            happened_at: record.happened_at.format(DISPLAY_FMT).to_string(),
            record_type: debt_kind_label(&record.kind).into(),
            account_name: account_names
                .get(&record.account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            subject: person_names
                .get(&record.person_id)
                .cloned()
                .unwrap_or_else(|| "已删除对象".into()),
            note: crypto::decrypt_string(dek, &record.note),
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
            edit_url: String::new(),
            delete_action: format!("/debts/{}/delete", record.id),
            delete_confirm: "确认删除这条借还记录？".into(),
            sort_key: record.happened_at,
        });
    }

    rows.sort_by(|left, right| right.sort_key.cmp(&left.sort_key));
    Ok(rows)
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
    category_is_food(state, dek, kind, name).await.map(|_| ())
}

pub(crate) async fn category_is_food(
    state: &AppState,
    dek: &crypto::Dek,
    kind: &str,
    name: &str,
) -> HandlerResult<bool> {
    category::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .find(|category| {
            category.kind == kind && crypto::decrypt_string(dek, &category.name) == name
        })
        .map(|category| category.is_food)
        .ok_or_else(|| bad_request("请选择设置中已有的对应收支分类"))
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
    let mut total_income: i64 = 0;
    let mut total_expense: i64 = 0;
    for b in bills {
        let cents = crypto::decrypt_cents(&dek, &b.amount);
        if b.kind == "income" {
            total_income += cents;
        } else {
            total_expense += cents;
        }
    }

    let html = BillsTemplate {
        records: ledger_rows(&state, &dek).await?,
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
    let redirect_to = parsed.redirect_to.clone();
    let is_food = category_is_food(&state, &dek, &parsed.kind, &parsed.category).await?;
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
        is_food: Set(is_food),
        note: Set(crypto::encrypt(&dek, parsed.note.as_bytes())),
        happened_at: Set(parsed.happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to(&redirect_to))
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
    let is_food = category_is_food(&state, &dek, &parsed.kind, &parsed.category).await?;
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
    active.is_food = Set(is_food);
    active.note = Set(crypto::encrypt(&dek, parsed.note.as_bytes()));
    active.happened_at = Set(parsed.happened_at);
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/bills"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteFormData>,
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
    Ok(Redirect::to(ledger_redirect(form.redirect_to.as_deref())))
}
