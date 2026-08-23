use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
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
    crypto, currency,
    entity::{account, account_detail, bill, debt_record, installment_item, transfer},
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
    page: usize,
    per_page: usize,
    total_pages: usize,
    total_records: usize,
}

struct AccountRow {
    id: i64,
    name: String,
    kind_label: String,
    card_number: String,
    account_username: String,
    credit_summary: String,
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
    currency: String,
    currencies: &'static [currency::CurrencyOption],
    card_number: String,
    account_username: String,
    credit_limit: String,
    billing_day: String,
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
    currency: String,
    card_number: String,
    account_username: String,
    credit_limit: String,
    billing_day: String,
    note: String,
}

#[derive(Deserialize)]
pub struct AccountBalanceFormData {
    balance: String,
}

#[derive(Default, Deserialize)]
pub struct AccountsQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
}

fn valid_account_kind(kind: &str) -> bool {
    matches!(
        kind,
        "payment"
            | "bank"
            | "stored_value"
            | "credit_card"
            | "credit_service"
            | "investment"
            | "other"
    )
}

fn account_kind_label(kind: &str) -> &'static str {
    match kind {
        "payment" => "支付账户",
        "bank" => "银行账户",
        "stored_value" => "储值卡账户",
        "credit_card" => "信用卡",
        "credit_service" => "信贷服务",
        "investment" => "投资账户",
        _ => "其他",
    }
}

async fn save_account_detail(
    state: &AppState,
    dek: &crypto::Dek,
    account_id: i64,
    kind: &str,
    card_number: &str,
    account_username: &str,
    credit_limit: i64,
    billing_day: i32,
) -> HandlerResult<()> {
    let (card_number, account_username, credit_limit, billing_day) = match kind {
        "payment" => ("", account_username.trim(), 0, 0),
        "bank" | "stored_value" => (card_number.trim(), "", 0, 0),
        "credit_card" => (card_number.trim(), "", credit_limit, billing_day),
        "credit_service" => ("", account_username.trim(), credit_limit, billing_day),
        _ => ("", "", 0, 0),
    };
    if card_number.is_empty() && account_username.is_empty() && credit_limit == 0 {
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
        credit_limit: Set(crypto::encrypt_cents(dek, credit_limit)),
        billing_day: Set(billing_day),
    })
    .on_conflict(
        OnConflict::column(account_detail::Column::AccountId)
            .update_columns([
                account_detail::Column::CardNumber,
                account_detail::Column::AccountUsername,
                account_detail::Column::CreditLimit,
                account_detail::Column::BillingDay,
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
        let from_cents = crypto::decrypt_cents(dek, &transfer.amount);
        let signed = if transfer.to_account_id == account_id {
            super::transfer_to_cents(dek, &transfer)
        } else {
            from_cents
                .checked_neg()
                .ok_or_else(|| err500("余额超出范围"))?
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

pub(crate) async fn current_balance(
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

pub(crate) async fn ensure_allowed_balance(
    state: &AppState,
    dek: &crypto::Dek,
    account_id: i64,
    balance: i64,
) -> HandlerResult<()> {
    let account = account::Entity::find_by_id(account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("账户不存在"))?;
    if !matches!(account.kind.as_str(), "credit_card" | "credit_service") {
        if balance < 0 {
            return Err(bad_request("该账户不允许透支"));
        }
        return Ok(());
    }
    let detail = account_detail::Entity::find_by_id(account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("信用账户缺少授信配置"))?;
    let limit = crypto::decrypt_cents(dek, &detail.credit_limit);
    let minimum = limit
        .checked_neg()
        .ok_or_else(|| err500("授信额超出范围"))?;
    if balance < minimum {
        return Err(bad_request("操作后将超过该账户的可用授信额度"));
    }
    Ok(())
}

pub(crate) async fn ensure_balance_delta(
    state: &AppState,
    dek: &crypto::Dek,
    account_id: i64,
    delta: i64,
) -> HandlerResult<()> {
    let projected = current_balance(state, dek, account_id)
        .await?
        .checked_add(delta)
        .ok_or_else(|| bad_request("余额超出范围"))?;
    ensure_allowed_balance(state, dek, account_id, projected).await
}

fn parse_balance(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("余额格式不正确"))?
        .round_dp(2);
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("余额超出范围"))
}

fn parse_credit_settings(kind: &str, limit: &str, day: &str) -> HandlerResult<(i64, i32)> {
    if kind != "credit_card" && kind != "credit_service" {
        return Ok((0, 0));
    }
    let decimal = Decimal::from_str(limit.trim())
        .map_err(|_| bad_request("授信额格式不正确"))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request("授信额必须大于 0"));
    }
    let credit_limit = (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("授信额超出范围"))?;
    let billing_day: i32 = day
        .trim()
        .parse()
        .map_err(|_| bad_request("账单日必须是 1 到 31"))?;
    if !(1..=31).contains(&billing_day) {
        return Err(bad_request("账单日必须是 1 到 31"));
    }
    Ok((credit_limit, billing_day))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<AccountsQuery>,
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
    let details: HashMap<i64, (String, String, i64, i32)> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| {
            let card_number = crypto::decrypt_string(&dek, &detail.card_number);
            let account_username = crypto::decrypt_string(&dek, &detail.account_username);
            let credit_limit = crypto::decrypt_cents(&dek, &detail.credit_limit);
            (
                detail.account_id,
                (
                    super::mask_card_number(&card_number),
                    super::mask_account_username(&account_username),
                    credit_limit,
                    detail.billing_day,
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
        let from_cents = crypto::decrypt_cents(&dek, &transfer.amount);
        let to_cents = super::transfer_to_cents(&dek, &transfer);
        let outgoing = from_cents
            .checked_neg()
            .ok_or_else(|| err500("余额超出范围"))?;
        for (account_id, signed) in [
            (transfer.from_account_id, outgoing),
            (transfer.to_account_id, to_cents),
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
            let (card_number, account_username, credit_limit, billing_day) =
                details.get(&account.id).cloned().unwrap_or_default();
            let credit_summary = if billing_day > 0 {
                format!(
                    "授信额 {} · 每月 {} 日",
                    currency::format(credit_limit, &account.currency),
                    billing_day
                )
            } else {
                String::new()
            };
            AccountRow {
                id: account.id,
                name: crypto::decrypt_string(&dek, &account.name),
                kind_label: account_kind_label(&account.kind).into(),
                card_number,
                account_username,
                credit_summary,
                note: crypto::decrypt_string(&dek, &account.note),
                balance: currency::format(
                    net.get(&account.id).copied().unwrap_or_default(),
                    &account.currency,
                ),
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

    let html = AccountsTemplate {
        accounts: rows,
        page: pagination.page,
        per_page: pagination.per_page,
        total_pages: pagination.total_pages,
        total_records,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_form(State(state): State<AppState>) -> HandlerResult<Html<String>> {
    let html = AccountFormTemplate {
        heading: "新建账户".into(),
        action: "/accounts".into(),
        name: String::new(),
        account_kind: DEFAULT_ACCOUNT_KIND.into(),
        currency: currency::default_currency(&state).await.map_err(err500)?,
        currencies: currency::CURRENCIES,
        card_number: String::new(),
        account_username: String::new(),
        credit_limit: String::new(),
        billing_day: String::new(),
        note: String::new(),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
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
    if !currency::valid(&form.currency) {
        return Err(bad_request("账户货币无效"));
    }
    let (credit_limit, billing_day) =
        parse_credit_settings(&form.account_kind, &form.credit_limit, &form.billing_day)?;
    let account = account::ActiveModel {
        name: Set(crypto::encrypt(&dek, form.name.trim().as_bytes())),
        kind: Set(form.account_kind.clone()),
        currency: Set(form.currency.clone()),
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
        credit_limit,
        billing_day,
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
    let credit_limit = detail
        .as_ref()
        .map(|detail| super::fmt_cents(crypto::decrypt_cents(&dek, &detail.credit_limit)))
        .unwrap_or_default();
    let billing_day = detail
        .as_ref()
        .filter(|detail| detail.billing_day > 0)
        .map(|detail| detail.billing_day.to_string())
        .unwrap_or_default();
    let html = AccountFormTemplate {
        heading: "编辑账户".into(),
        action: format!("/accounts/{id}/edit"),
        name: crypto::decrypt_string(&dek, &account.name),
        account_kind: account.kind,
        currency: account.currency,
        currencies: currency::CURRENCIES,
        card_number,
        account_username,
        credit_limit,
        billing_day,
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
    let _balance_guard = state.balance_writes.lock().await;
    if form.name.trim().is_empty() {
        return Err(bad_request("账户名不能为空"));
    }
    if !valid_account_kind(&form.account_kind) {
        return Err(bad_request("账户类型无效"));
    }
    if !currency::valid(&form.currency) {
        return Err(bad_request("账户货币无效"));
    }
    let (credit_limit, billing_day) =
        parse_credit_settings(&form.account_kind, &form.credit_limit, &form.billing_day)?;
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let balance = current_balance(&state, &dek, id).await?;
    if account.currency != form.currency {
        let has_activity = bill::Entity::find()
            .filter(bill::Column::AccountId.eq(id))
            .one(&state.db)
            .await
            .map_err(err500)?
            .is_some()
            || transfer::Entity::find()
                .filter(
                    Condition::any()
                        .add(transfer::Column::FromAccountId.eq(id))
                        .add(transfer::Column::ToAccountId.eq(id)),
                )
                .one(&state.db)
                .await
                .map_err(err500)?
                .is_some()
            || debt_record::Entity::find()
                .filter(debt_record::Column::AccountId.eq(id))
                .one(&state.db)
                .await
                .map_err(err500)?
                .is_some()
            || crypto::decrypt_cents(&dek, &account.balance_offset) != 0;
        if has_activity {
            return Err(bad_request(
                "已有余额或流水的账户不能修改货币；请新建对应货币账户后转账",
            ));
        }
    }
    if matches!(form.account_kind.as_str(), "credit_card" | "credit_service") {
        let minimum = credit_limit
            .checked_neg()
            .ok_or_else(|| bad_request("授信额超出范围"))?;
        if balance < minimum {
            return Err(bad_request("当前透支金额超过新的授信额"));
        }
    } else if balance < 0 {
        return Err(bad_request("当前余额为负，不能改为禁止透支的账户类型"));
    }
    let mut active = account.into_active_model();
    active.name = Set(crypto::encrypt(&dek, form.name.trim().as_bytes()));
    active.kind = Set(form.account_kind.clone());
    active.currency = Set(form.currency.clone());
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    save_account_detail(
        &state,
        &dek,
        id,
        &form.account_kind,
        &form.card_number,
        &form.account_username,
        credit_limit,
        billing_day,
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
        current_balance: currency::format(
            current_balance(&state, &dek, id).await?,
            &account.currency,
        ),
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
    let _balance_guard = state.balance_writes.lock().await;
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let target = parse_balance(&form.balance)?;
    ensure_allowed_balance(&state, &dek, id, target).await?;
    let offset = target
        .checked_sub(transaction_balance(&state, &dek, id).await?)
        .ok_or_else(|| bad_request("余额超出范围"))?;
    let mut active = account.into_active_model();
    active.balance_offset = Set(crypto::encrypt_cents(&dek, offset));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/accounts"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    if account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, "账户不存在".into()));
    }
    let mut balance_changes: HashMap<i64, i64> = HashMap::new();
    for transfer in transfer::Entity::find()
        .filter(
            Condition::any()
                .add(transfer::Column::FromAccountId.eq(id))
                .add(transfer::Column::ToAccountId.eq(id)),
        )
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let from_amount = crypto::decrypt_cents(&dek, &transfer.amount);
        let to_amount = super::transfer_to_cents(&dek, &transfer);
        let (other_id, delta) = if transfer.from_account_id == id {
            (
                transfer.to_account_id,
                to_amount
                    .checked_neg()
                    .ok_or_else(|| bad_request("金额超出范围"))?,
            )
        } else {
            (transfer.from_account_id, from_amount)
        };
        if other_id != id {
            let current = balance_changes.get(&other_id).copied().unwrap_or_default();
            balance_changes.insert(
                other_id,
                current
                    .checked_add(delta)
                    .ok_or_else(|| bad_request("余额超出范围"))?,
            );
        }
    }
    for (account_id, delta) in balance_changes {
        ensure_balance_delta(&state, &dek, account_id, delta).await?;
    }
    for item in installment_item::Entity::find()
        .filter(installment_item::Column::RepaymentAccountId.eq(id))
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let mut active = item.into_active_model();
        active.paid_at = Set(None);
        active.repayment_account_id = Set(None);
        active.principal_transfer_id = Set(None);
        active.charge_bill_id = Set(None);
        active.update(&state.db).await.map_err(err500)?;
    }
    account::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/accounts"))
}
