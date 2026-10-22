use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{account, account_detail, bill, category, installment_plan},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const REFUND_CATEGORY: &str = "退款收入";

/// Refunds remain real cash flows, but statistics attribute them to the original expense.
pub(crate) struct RefundTotals(HashMap<i64, i64>);

impl RefundTotals {
    pub(crate) fn new<'a>(
        dek: &crypto::Dek,
        bills: impl IntoIterator<Item = &'a bill::Model>,
    ) -> Self {
        let mut totals = HashMap::new();
        for item in bills {
            if let Some(original) = item.refund_of_bill_id {
                let total = totals.entry(original).or_insert(0_i64);
                *total = total.saturating_add(crypto::decrypt_cents(dek, &item.amount));
            }
        }
        Self(totals)
    }

    pub(crate) fn amount(&self, dek: &crypto::Dek, item: &bill::Model) -> i64 {
        if item.refund_of_bill_id.is_some() || !matches!(item.kind.as_str(), "income" | "expense") {
            return 0;
        }
        let amount = crypto::decrypt_cents(dek, &item.amount);
        if item.kind == "expense" {
            amount
                .saturating_sub(self.0.get(&item.id).copied().unwrap_or(0))
                .max(0)
        } else {
            amount
        }
    }

    pub(crate) fn status(&self, dek: &crypto::Dek, item: &bill::Model) -> &'static str {
        if self.0.contains_key(&item.id) {
            if self.amount(dek, item) == 0 {
                "已退款"
            } else {
                "部分退款"
            }
        } else {
            ""
        }
    }
}

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

#[derive(Default, Deserialize)]
pub struct RefundQuery {
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
pub struct RefundFormData {
    mode: String,
    #[serde(default)]
    amount: String,
    happened_at: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Serialize)]
struct RefundResponse {
    ok: bool,
    message: String,
    redirect: String,
}

#[derive(Template)]
#[template(path = "bill_refund.html")]
struct RefundTemplate {
    id: i64,
    account_name: String,
    currency: String,
    original_category: String,
    original_note: String,
    original_happened_at: String,
    original_amount: String,
    refunded_amount: String,
    remaining_amount: String,
    happened_at: String,
    is_private: bool,
    has_installment: bool,
    can_refund: bool,
    redirect_to: String,
}

fn ledger_redirect(value: Option<&str>, is_private: bool) -> &'static str {
    if is_private {
        return "/privacy/bills";
    }
    match value {
        Some("/dashboard") => "/dashboard",
        Some("/bills/new") => "/bills/new",
        _ => "/bills",
    }
}

fn positive_cents(dek: &crypto::Dek, encrypted: &str) -> HandlerResult<i64> {
    let amount = crypto::decrypt_string(dek, encrypted)
        .parse::<i64>()
        .map_err(|_| err500("账单金额无效"))?;
    if amount <= 0 {
        return Err(err500("账单金额必须大于 0"));
    }
    Ok(amount)
}

fn parse_partial_amount(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim()).map_err(|_| bad_request("金额格式不正确"))?;
    if decimal <= Decimal::ZERO {
        return Err(bad_request("退款金额必须大于 0"));
    }
    let cents = decimal
        .checked_mul(Decimal::from(100))
        .ok_or_else(|| bad_request("退款金额超出范围"))?;
    if !cents.fract().is_zero() {
        return Err(bad_request("退款金额最多保留两位小数"));
    }
    cents
        .to_i64()
        .ok_or_else(|| bad_request("退款金额超出范围"))
}

fn ensure_refundable(original: &bill::Model) -> HandlerResult<()> {
    if original.kind != "expense" || original.refund_of_bill_id.is_some() {
        return Err(bad_request("只有原始支出账单可以退款"));
    }
    Ok(())
}

pub(crate) async fn refund_totals<C: ConnectionTrait>(
    db: &C,
    dek: &crypto::Dek,
    original: &bill::Model,
) -> HandlerResult<(i64, i64, i64)> {
    let amount = positive_cents(dek, &original.amount)?;
    let mut refunded = 0_i64;
    for refund in bill::Entity::find()
        .filter(bill::Column::RefundOfBillId.eq(original.id))
        .all(db)
        .await
        .map_err(err500)?
    {
        refunded = refunded
            .checked_add(positive_cents(dek, &refund.amount)?)
            .ok_or_else(|| err500("已退金额超出范围"))?;
    }
    let remaining = amount
        .checked_sub(refunded)
        .filter(|value| *value >= 0)
        .ok_or_else(|| err500("已退金额超过原始支出"))?;
    Ok((amount, refunded, remaining))
}

async fn has_refunds(state: &AppState, id: i64) -> HandlerResult<bool> {
    Ok(bill::Entity::find()
        .filter(bill::Column::RefundOfBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some())
}

pub(crate) async fn ensure_editable(state: &AppState, bill: &bill::Model) -> HandlerResult<()> {
    if bill.refund_of_bill_id.is_some() {
        return Err(bad_request("退款收入不能编辑；如需更正，请删除后重新退款"));
    }
    if has_refunds(state, bill.id).await? {
        return Err(bad_request(
            "已有退款的原账单不能编辑，请先删除全部退款记录",
        ));
    }
    Ok(())
}

pub(crate) async fn ensure_deletable(state: &AppState, id: i64) -> HandlerResult<()> {
    if has_refunds(state, id).await? {
        return Err(bad_request(
            "已有退款的原账单不能删除，请先删除全部退款记录",
        ));
    }
    Ok(())
}

async fn refund_category<C: ConnectionTrait>(db: &C, dek: &crypto::Dek) -> HandlerResult<i64> {
    if let Some(category) = category::Entity::find()
        .filter(category::Column::Kind.eq("income"))
        .all(db)
        .await
        .map_err(err500)?
        .into_iter()
        .find(|item| crypto::decrypt_string(dek, &item.name) == REFUND_CATEGORY)
    {
        return Ok(category.id);
    }
    Ok(category::ActiveModel {
        kind: Set("income".into()),
        name: Set(crypto::encrypt(dek, REFUND_CATEGORY.as_bytes())),
        is_food: Set(false),
        count_limit: Set(String::new()),
        count_limit_period: Set("month".into()),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(err500)?
    .id)
}

/// Record a real refund inside the caller's financial transaction.
pub(crate) async fn record_refund_on(
    tx: &sea_orm::DatabaseTransaction,
    dek: &crypto::Dek,
    original: &bill::Model,
    unlocked: bool,
    amount: i64,
    happened_at: NaiveDateTime,
    note: &str,
) -> HandlerResult<bill::Model> {
    super::bills::require_bill_access(original, unlocked)?;
    ensure_refundable(original)?;
    let (_, _, remaining) = refund_totals(tx, dek, original).await?;
    if amount <= 0 || amount > remaining {
        return Err(bad_request("退款金额必须大于 0 且不能超过剩余可退金额"));
    }
    super::accounts::ensure_balance_delta_on(tx, dek, original.account_id, amount).await?;
    let category_id = refund_category(tx, dek).await?;
    bill::ActiveModel {
        account_id: Set(original.account_id),
        kind: Set("income".into()),
        amount: Set(crypto::encrypt_cents(dek, amount)),
        category: Set(crypto::encrypt(dek, REFUND_CATEGORY.as_bytes())),
        category_id: Set(Some(category_id)),
        is_food: Set(false),
        is_private: Set(original.is_private),
        refund_of_bill_id: Set(Some(original.id)),
        note: Set(crypto::encrypt(dek, note.trim().as_bytes())),
        happened_at: Set(happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(tx)
    .await
    .map_err(err500)
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(crate::privacy::PrivacyAccess(unlocked)): Extension<crate::privacy::PrivacyAccess>,
    Path(id): Path<i64>,
    Query(query): Query<RefundQuery>,
) -> HandlerResult<Html<String>> {
    let original = bill::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    super::bills::require_bill_access(&original, unlocked)?;
    ensure_refundable(&original)?;
    let account = account::Entity::find_by_id(original.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原账户不存在"))?;
    let details = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .map(|detail| (detail.account_id, detail))
        .into_iter()
        .collect::<HashMap<_, _>>();
    let names = super::bills::account_display_names(&dek, std::slice::from_ref(&account), &details);
    let (amount, refunded, remaining) = refund_totals(&state.db, &dek, &original).await?;
    let html = RefundTemplate {
        id,
        account_name: names.get(&account.id).cloned().unwrap_or_default(),
        currency: account.currency.clone(),
        original_category: crypto::decrypt_string(&dek, &original.category),
        original_note: crypto::decrypt_string(&dek, &original.note),
        original_happened_at: original.happened_at.format(TIME_FMT).to_string(),
        original_amount: currency::format(amount, &account.currency),
        refunded_amount: currency::format(refunded, &account.currency),
        remaining_amount: currency::format(remaining, &account.currency),
        happened_at: chrono::Utc::now().format(TIME_FMT).to_string(),
        is_private: original.is_private,
        has_installment: installment_plan::Entity::find()
            .filter(installment_plan::Column::BillId.eq(id))
            .filter(installment_plan::Column::DeletedAt.is_null())
            .one(&state.db)
            .await
            .map_err(err500)?
            .is_some(),
        can_refund: remaining > 0,
        redirect_to: ledger_redirect(query.redirect_to.as_deref(), original.is_private).into(),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(crate::privacy::PrivacyAccess(unlocked)): Extension<crate::privacy::PrivacyAccess>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<RefundFormData>,
) -> HandlerResult<Response> {
    let _balance_guard = state.balance_writes.lock().await;
    let transaction = state.db.begin().await.map_err(err500)?;
    let original = bill::Entity::find_by_id(id)
        .one(&transaction)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    super::bills::require_bill_access(&original, unlocked)?;
    ensure_refundable(&original)?;
    let (_, _, remaining) = refund_totals(&transaction, &dek, &original).await?;
    if remaining == 0 {
        return Err(bad_request("该账单已全额退款"));
    }
    let amount = match form.mode.as_str() {
        "full" => remaining,
        "partial" => parse_partial_amount(&form.amount)?,
        _ => return Err(bad_request("请选择全额退款或部分退款")),
    };
    if amount > remaining {
        return Err(bad_request("退款金额不能超过剩余可退金额"));
    }
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    record_refund_on(
        &transaction,
        &dek,
        &original,
        unlocked,
        amount,
        happened_at,
        &form.note,
    )
    .await?;
    transaction.commit().await.map_err(err500)?;
    let redirect = ledger_redirect(form.redirect_to.as_deref(), original.is_private);
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
    {
        Ok(Json(RefundResponse {
            ok: true,
            message: "退款已记录并冲减原支出，原资金记录保留".into(),
            redirect: redirect.into(),
        })
        .into_response())
    } else {
        Ok(Redirect::to(redirect).into_response())
    }
}
