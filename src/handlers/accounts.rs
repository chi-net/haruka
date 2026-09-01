use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect},
    Form, Json,
};
use chrono::Datelike;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, ColumnTrait, Condition, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{
        account, account_detail, balance_adjustment, bill, debt_person, debt_record,
        installment_item, installment_plan, transfer,
    },
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
    per_page: usize,
    pagination: super::PaginationView,
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

#[derive(Serialize)]
pub struct AccountBalanceResponse {
    ok: bool,
    account_id: i64,
    balance: String,
    currency: String,
    kind: String,
    can_transfer_out: bool,
}

struct AccountLedgerRow {
    happened_at: String,
    record_type: String,
    subject: String,
    note: String,
    amount: String,
    money_class: String,
    detail_url: String,
}

#[derive(Template)]
#[template(path = "account_detail.html")]
struct AccountDetailTemplate {
    id: i64,
    name: String,
    kind_label: String,
    is_investment: bool,
    currency: String,
    card_number_masked: String,
    account_username: String,
    credit_summary: String,
    note: String,
    created_at: String,
    balance: String,
    month_label: String,
    month_income: String,
    month_expense: String,
    month_net: String,
    total_income: String,
    total_expense: String,
    records: Vec<AccountLedgerRow>,
    per_page: usize,
    pagination: super::PaginationView,
}

#[derive(Serialize)]
pub struct CardNumberResponse {
    card_number: String,
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
    heading: String,
    account_name: String,
    current_balance: String,
    action: String,
    help: String,
    confirm_message: String,
    button_label: String,
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

#[derive(Default, Deserialize)]
pub struct AccountDetailQuery {
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
    if !matches!(kind, "credit_card" | "credit_service")
        && card_number.is_empty()
        && account_username.is_empty()
        && credit_limit == 0
    {
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
    if account.kind == "credit_service" && balance > 0 {
        return Err(bad_request("信贷服务余额不能高于 0"));
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

pub async fn balance_summary(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<impl IntoResponse> {
    let account = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let balance = current_balance(&state, &dek, id).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(AccountBalanceResponse {
            ok: true,
            account_id: id,
            balance: currency::format(balance, &account.currency),
            currency: account.currency,
            kind: account.kind,
            can_transfer_out: balance > 0,
        }),
    ))
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
    if decimal < Decimal::ZERO {
        return Err(bad_request("授信额不能小于 0"));
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
        per_page: pagination.per_page,
        pagination: super::pagination_view(
            &pagination,
            total_records,
            "/accounts",
            "个账户",
            std::iter::empty::<(String, String)>(),
        ),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn detail(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Query(query): Query<AccountDetailQuery>,
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
    let account_bills = bill::Entity::find()
        .filter(bill::Column::AccountId.eq(id))
        .order_by_desc(bill::Column::HappenedAt)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_transfers = transfer::Entity::find()
        .filter(
            Condition::any()
                .add(transfer::Column::FromAccountId.eq(id))
                .add(transfer::Column::ToAccountId.eq(id)),
        )
        .order_by_desc(transfer::Column::HappenedAt)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_debts = debt_record::Entity::find()
        .filter(debt_record::Column::AccountId.eq(id))
        .order_by_desc(debt_record::Column::HappenedAt)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_adjustments = balance_adjustment::Entity::find()
        .filter(balance_adjustment::Column::AccountId.eq(id))
        .order_by_desc(balance_adjustment::Column::HappenedAt)
        .all(&state.db)
        .await
        .map_err(err500)?;

    let today = chrono::Local::now().date_naive();
    let month_start = chrono::NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
        .ok_or_else(|| err500("无法计算本月开始日期"))?;
    let mut month_income = 0i64;
    let mut month_expense = 0i64;
    let mut total_income = 0i64;
    let mut total_expense = 0i64;
    for item in &account_bills {
        let amount = crypto::decrypt_cents(&dek, &item.amount);
        let total = if item.kind == "income" {
            &mut total_income
        } else {
            &mut total_expense
        };
        *total = total
            .checked_add(amount)
            .ok_or_else(|| err500("账户收支汇总金额超出范围"))?;
        if item.happened_at.date() >= month_start && item.happened_at.date() <= today {
            let monthly = if item.kind == "income" {
                &mut month_income
            } else {
                &mut month_expense
            };
            *monthly = monthly
                .checked_add(amount)
                .ok_or_else(|| err500("账户月度汇总金额超出范围"))?;
        }
    }
    let month_net = month_income
        .checked_sub(month_expense)
        .ok_or_else(|| err500("账户月结余超出范围"))?;

    let other_account_names: HashMap<i64, String> = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|item| (item.id, crypto::decrypt_string(&dek, &item.name)))
        .collect();
    let person_names: HashMap<i64, String> = debt_person::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|person| (person.id, crypto::decrypt_string(&dek, &person.name)))
        .collect();
    let installment_plans: HashMap<i64, i64> = installment_plan::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|plan| (plan.bill_id, plan.id))
        .collect();

    let mut records = Vec::with_capacity(
        account_bills.len()
            + account_transfers.len()
            + account_debts.len()
            + account_adjustments.len(),
    );
    for item in account_bills {
        let incoming = item.kind == "income";
        let amount = crypto::decrypt_cents(&dek, &item.amount);
        records.push((
            item.happened_at,
            AccountLedgerRow {
                happened_at: item.happened_at.format("%Y-%m-%dT%H:%M").to_string(),
                record_type: if incoming {
                    "收入".into()
                } else if installment_plans.contains_key(&item.id) {
                    "支出 · 分期".into()
                } else {
                    "支出".into()
                },
                subject: crypto::decrypt_string(&dek, &item.category),
                note: crypto::decrypt_string(&dek, &item.note),
                amount: format!(
                    "{}{}",
                    if incoming { "+" } else { "-" },
                    currency::format(amount, &account.currency)
                ),
                money_class: if incoming {
                    "text-green-600"
                } else {
                    "text-red-600"
                }
                .into(),
                detail_url: installment_plans
                    .get(&item.id)
                    .map(|plan_id| format!("/installments/{plan_id}"))
                    .unwrap_or_default(),
            },
        ));
    }
    for item in account_transfers {
        let incoming = item.to_account_id == id;
        let amount = if incoming {
            super::transfer_to_cents(&dek, &item)
        } else {
            crypto::decrypt_cents(&dek, &item.amount)
        };
        let counterpart_id = if incoming {
            item.from_account_id
        } else {
            item.to_account_id
        };
        let counterpart = other_account_names
            .get(&counterpart_id)
            .cloned()
            .unwrap_or_else(|| "已删除账户".into());
        records.push((
            item.happened_at,
            AccountLedgerRow {
                happened_at: item.happened_at.format("%Y-%m-%dT%H:%M").to_string(),
                record_type: if incoming {
                    "转入".into()
                } else {
                    "转出".into()
                },
                subject: if incoming {
                    format!("来自 {counterpart}")
                } else {
                    format!("转至 {counterpart}")
                },
                note: crypto::decrypt_string(&dek, &item.note),
                amount: format!(
                    "{}{}",
                    if incoming { "+" } else { "-" },
                    currency::format(amount, &account.currency)
                ),
                money_class: if incoming {
                    "text-green-600"
                } else {
                    "text-red-600"
                }
                .into(),
                detail_url: String::new(),
            },
        ));
    }
    for item in account_debts {
        let incoming = matches!(item.kind.as_str(), "borrow" | "repayment_received");
        let amount = crypto::decrypt_cents(&dek, &item.amount);
        records.push((
            item.happened_at,
            AccountLedgerRow {
                happened_at: item.happened_at.format("%Y-%m-%dT%H:%M").to_string(),
                record_type: match item.kind.as_str() {
                    "lend" => "借出",
                    "borrow" => "借入",
                    "repayment_received" => "收回还款",
                    "repayment_paid" => "归还借款",
                    _ => "借还",
                }
                .into(),
                subject: person_names
                    .get(&item.person_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除对象".into()),
                note: crypto::decrypt_string(&dek, &item.note),
                amount: format!(
                    "{}{}",
                    if incoming { "+" } else { "-" },
                    currency::format(amount, &account.currency)
                ),
                money_class: if incoming {
                    "text-green-600"
                } else {
                    "text-red-600"
                }
                .into(),
                detail_url: String::new(),
            },
        ));
    }
    for item in account_adjustments {
        let from_balance = crypto::decrypt_cents(&dek, &item.from_balance);
        let to_balance = crypto::decrypt_cents(&dek, &item.to_balance);
        records.push((
            item.happened_at,
            AccountLedgerRow {
                happened_at: item.happened_at.format("%Y-%m-%dT%H:%M").to_string(),
                record_type: "余额调整".into(),
                subject: "强制设置余额".into(),
                note: format!(
                    "{} → {}",
                    currency::format(from_balance, &account.currency),
                    currency::format(to_balance, &account.currency)
                ),
                amount: currency::format(to_balance, &account.currency),
                money_class: "text-amber-700".into(),
                detail_url: String::new(),
            },
        ));
    }
    records.sort_by(|left, right| right.0.cmp(&left.0));
    let records = records.into_iter().map(|(_, row)| row).collect::<Vec<_>>();
    let total_records = records.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let records = records
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();

    let card_number = detail
        .as_ref()
        .map(|item| crypto::decrypt_string(&dek, &item.card_number))
        .unwrap_or_default();
    let account_username = detail
        .as_ref()
        .map(|item| crypto::decrypt_string(&dek, &item.account_username))
        .unwrap_or_default();
    let credit_summary = detail
        .as_ref()
        .filter(|item| item.billing_day > 0)
        .map(|item| {
            format!(
                "授信额 {} · 每月 {} 日出账",
                currency::format(
                    crypto::decrypt_cents(&dek, &item.credit_limit),
                    &account.currency
                ),
                item.billing_day
            )
        })
        .unwrap_or_default();
    let html = AccountDetailTemplate {
        id,
        name: crypto::decrypt_string(&dek, &account.name),
        kind_label: account_kind_label(&account.kind).into(),
        is_investment: account.kind == "investment",
        currency: account.currency.clone(),
        card_number_masked: if card_number.is_empty() {
            String::new()
        } else {
            super::mask_card_number(&card_number)
        },
        account_username,
        credit_summary,
        note: crypto::decrypt_string(&dek, &account.note),
        created_at: account.created_at.format("%Y-%m-%dT%H:%M").to_string(),
        balance: currency::format(current_balance(&state, &dek, id).await?, &account.currency),
        month_label: format!("{} 年 {} 月", today.year(), today.month()),
        month_income: currency::format(month_income, &account.currency),
        month_expense: currency::format(month_expense, &account.currency),
        month_net: currency::format(month_net, &account.currency),
        total_income: currency::format(total_income, &account.currency),
        total_expense: currency::format(total_expense, &account.currency),
        records,
        per_page: pagination.per_page,
        pagination: super::pagination_view(
            &pagination,
            total_records,
            &format!("/accounts/{id}"),
            "条流水",
            std::iter::empty::<(String, String)>(),
        ),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn card_number(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<impl IntoResponse> {
    account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    let detail = account_detail::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "这个账户没有卡号".into()))?;
    let card_number = crypto::decrypt_string(&dek, &detail.card_number);
    if card_number.is_empty() {
        return Err((StatusCode::NOT_FOUND, "这个账户没有卡号".into()));
    }
    Ok((
        [("cache-control", "no-store"), ("pragma", "no-cache")],
        Json(CardNumberResponse { card_number }),
    ))
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
            || balance_adjustment::Entity::find()
                .filter(balance_adjustment::Column::AccountId.eq(id))
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
        if form.account_kind == "credit_service" && balance > 0 {
            return Err(bad_request(
                "信贷服务余额不能高于 0；请先把正余额转出后再修改账户",
            ));
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
        heading: if account.kind == "investment" {
            "校准持仓价值"
        } else {
            "设置账户余额"
        }
        .into(),
        account_name: crypto::decrypt_string(&dek, &account.name),
        current_balance: currency::format(
            current_balance(&state, &dek, id).await?,
            &account.currency,
        ),
        action: format!("/accounts/{id}/balance"),
        help: if account.kind == "investment" {
            "填写本月查看到的实际持仓总价值。高于当前值会增加账面价值，低于当前值会扣减；差额记录为余额调整，不计入普通收入或支出。"
        } else {
            "最多保留两位小数，并会在账单中新增一条不可删除的“余额调整”流水。普通账户不允许负数；信用账户不能低于负授信额。"
        }
        .into(),
        confirm_message: if account.kind == "investment" {
            "确认按当前持仓价值校准该投资账户？"
        } else {
            "确认强制设置该账户的余额？"
        }
        .into(),
        button_label: if account.kind == "investment" {
            "确认校准"
        } else {
            "设置余额"
        }
        .into(),
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
    let previous = current_balance(&state, &dek, id).await?;
    let offset = target
        .checked_sub(transaction_balance(&state, &dek, id).await?)
        .ok_or_else(|| bad_request("余额超出范围"))?;
    let now = chrono::Utc::now().naive_utc();
    let transaction = state.db.begin().await.map_err(err500)?;
    let mut active = account.into_active_model();
    active.balance_offset = Set(crypto::encrypt_cents(&dek, offset));
    active.update(&transaction).await.map_err(err500)?;
    balance_adjustment::ActiveModel {
        account_id: Set(id),
        from_balance: Set(crypto::encrypt_cents(&dek, previous)),
        to_balance: Set(crypto::encrypt_cents(&dek, target)),
        happened_at: Set(now),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to(&format!("/accounts/{id}")))
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
