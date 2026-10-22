use std::{collections::HashMap, str::FromStr};

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
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

use crate::{
    credit_statements::{self, Ledger},
    crypto,
    entity::{
        account, account_detail, balance_adjustment, bill, debt_record, installment_item,
        installment_plan, transfer,
    },
    installment_state::RefundContext,
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}
fn credit(account: &account::Model) -> bool {
    matches!(account.kind.as_str(), "credit_card" | "credit_service")
}
fn add(left: i64, right: i64) -> HandlerResult<i64> {
    left.checked_add(right)
        .ok_or_else(|| bad_request("还款金额超出范围"))
}
fn amount(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("金额格式不正确"))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request("金额必须大于 0"));
    }
    decimal
        .checked_mul(Decimal::from(100))
        .and_then(|value| value.to_i64())
        .ok_or_else(|| bad_request("金额超出范围"))
}
fn snapshot(value: &str, expected: i64) -> HandlerResult<()> {
    if value.parse::<i64>().ok() != Some(expected) {
        return Err(bad_request("欠款或分期计划已变化，请刷新后重新确认"));
    }
    Ok(())
}
fn redirect(value: Option<&str>) -> &str {
    match value {
        Some("/bills/new") => "/bills/new",
        Some("/bills") => "/bills",
        Some("/installments") => "/installments",
        _ => "/dashboard",
    }
}

struct Snapshot {
    bills: Vec<bill::Model>,
    transfers: Vec<transfer::Model>,
    debts: Vec<debt_record::Model>,
    adjustments: Vec<balance_adjustment::Model>,
    plans: HashMap<i64, installment_plan::Model>,
    items: Vec<installment_item::Model>,
}
impl Snapshot {
    async fn load<C: ConnectionTrait>(db: &C) -> HandlerResult<Self> {
        Ok(Self {
            bills: bill::Entity::find().all(db).await.map_err(err500)?,
            transfers: transfer::Entity::find().all(db).await.map_err(err500)?,
            debts: debt_record::Entity::find().all(db).await.map_err(err500)?,
            adjustments: balance_adjustment::Entity::find()
                .all(db)
                .await
                .map_err(err500)?,
            plans: installment_plan::Entity::find()
                .all(db)
                .await
                .map_err(err500)?
                .into_iter()
                .map(|plan| (plan.id, plan))
                .collect(),
            items: installment_item::Entity::find()
                .order_by_asc(installment_item::Column::DueDate)
                .order_by_asc(installment_item::Column::Sequence)
                .order_by_asc(installment_item::Column::Id)
                .all(db)
                .await
                .map_err(err500)?,
        })
    }
    fn ordinary(
        &self,
        account: &account::Model,
        detail: Option<&account_detail::Model>,
        dek: &crypto::Dek,
        time_zone: super::ClientTimeZone,
    ) -> HandlerResult<i64> {
        let statement = credit_statements::calculate(
            account,
            detail,
            &Ledger {
                bills: &self.bills,
                transfers: &self.transfers,
                debts: &self.debts,
                adjustments: &self.adjustments,
                plans: &self.plans,
                items: &self.items,
            },
            dek,
            time_zone,
            time_zone.today(),
        )
        .map_err(|error| bad_request(&error))?;
        statement
            .due
            .iter()
            .try_fold(statement.unbilled, |total, due| add(total, due.ordinary))
    }
    fn options(
        &self,
        account: &account::Model,
        ordinary: i64,
        dek: &crypto::Dek,
    ) -> HandlerResult<OptionsResponse> {
        let refunds = RefundContext::new(dek, &self.bills).map_err(|error| bad_request(&error))?;
        let mut plans = Vec::new();
        for plan in self
            .plans
            .values()
            .filter(|plan| plan.account_id == account.id && plan.deleted_at.is_none())
        {
            let status = refunds
                .state(dek, plan)
                .map_err(|error| bad_request(&error))?;
            let state = if status.full_refund {
                "refunded"
            } else if status.needs_review {
                "needs_review"
            } else {
                "active"
            };
            let mut remaining = 0;
            let mut next = None;
            for item in self
                .items
                .iter()
                .filter(|item| item.plan_id == plan.id && item.paid_at.is_none())
            {
                let total = crypto::decrypt_cents(dek, &item.total);
                if total < 0 {
                    return Err(bad_request("分期金额无效"));
                }
                remaining = add(remaining, total)?;
                if next.is_none() {
                    next = Some((item.id, total));
                }
            }
            let title = match plan
                .bill_id
                .and_then(|id| self.bills.iter().find(|bill| bill.id == id))
            {
                Some(bill) if bill.is_private => "隐私账单".into(),
                Some(bill) => {
                    let category = crypto::decrypt_string(dek, &bill.category);
                    let note = crypto::decrypt_string(dek, &bill.note);
                    if note.is_empty() {
                        category
                    } else {
                        format!("{category} · {note}")
                    }
                }
                None => "最低还款余款分期".into(),
            };
            let active = state == "active";
            let next_cents = if active {
                next.map_or(0, |(_, total)| total)
            } else {
                0
            };
            let remaining = if active { remaining } else { 0 };
            plans.push(PlanOption {
                id: plan.id,
                title,
                state: state.into(),
                next_item_id: if active { next.map(|(id, _)| id) } else { None },
                next_cents: next_cents.to_string(),
                next_amount: super::fmt_cents(next_cents),
                remaining_cents: remaining.to_string(),
                remaining_amount: if status.needs_review {
                    "待核对".into()
                } else {
                    super::fmt_cents(remaining)
                },
                can_cancel: plan.bill_id.is_none()
                    && !self
                        .items
                        .iter()
                        .any(|item| item.plan_id == plan.id && item.paid_at.is_some()),
            });
        }
        plans.sort_unstable_by_key(|plan| plan.id);
        Ok(OptionsResponse {
            ok: true,
            account_id: account.id,
            currency: account.currency.clone(),
            allow_overpayment: account.kind == "credit_card",
            ordinary_cents: ordinary.to_string(),
            ordinary_amount: super::fmt_cents(ordinary),
            plans,
        })
    }
}

#[derive(Deserialize)]
pub struct OptionsQuery {
    account_id: i64,
}
#[derive(Serialize)]
pub struct PlanOption {
    id: i64,
    title: String,
    state: String,
    next_item_id: Option<i64>,
    next_cents: String,
    next_amount: String,
    remaining_cents: String,
    remaining_amount: String,
    #[serde(skip)]
    can_cancel: bool,
}
#[derive(Serialize)]
pub struct OptionsResponse {
    ok: bool,
    account_id: i64,
    currency: String,
    allow_overpayment: bool,
    ordinary_cents: String,
    ordinary_amount: String,
    plans: Vec<PlanOption>,
}

pub async fn options(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Query(query): Query<OptionsQuery>,
) -> HandlerResult<impl IntoResponse> {
    let tx = state.db.begin().await.map_err(err500)?;
    let target = account::Entity::find_by_id(query.account_id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("还款账户不存在"))?;
    if !credit(&target) {
        return Err(bad_request("请选择信用账户还款"));
    }
    let detail = account_detail::Entity::find_by_id(target.id)
        .one(&tx)
        .await
        .map_err(err500)?;
    let data = Snapshot::load(&tx).await?;
    let ordinary = data.ordinary(&target, detail.as_ref(), &dek, time_zone)?;
    let response = data.options(&target, ordinary, &dek)?;
    tx.commit().await.map_err(err500)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(response)))
}

#[derive(Default, Deserialize)]
pub struct RepaymentForm {
    mode: String,
    from_account_id: i64,
    to_account_id: i64,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    plan_id: Option<i64>,
    #[serde(default)]
    item_id: Option<i64>,
    #[serde(default)]
    ordinary_cents: String,
    #[serde(default)]
    remaining_cents: String,
    happened_at: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    redirect_to: Option<String>,
    #[serde(default)]
    installment_term: String,
    #[serde(default)]
    installment_method: String,
    #[serde(default)]
    installment_annual_rate: String,
    #[serde(default)]
    installment_fee: String,
    #[serde(default)]
    installment_first_due_date: String,
}
#[derive(Serialize)]
struct CreateResponse {
    ok: bool,
    message: &'static str,
    redirect: String,
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    headers: HeaderMap,
    Form(form): Form<RepaymentForm>,
) -> HandlerResult<Response> {
    let _guard = state.balance_writes.lock().await;
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确（使用 UTC）"))?;
    let tx = state.db.begin().await.map_err(err500)?;
    let from = account::Entity::find_by_id(form.from_account_id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("还款来源不存在"))?;
    let to = account::Entity::find_by_id(form.to_account_id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("还款目标不存在"))?;
    if credit(&from) || !credit(&to) || from.id == to.id {
        return Err(bad_request("还款须从非信用账户转入信用账户"));
    }
    if from.currency != to.currency {
        return Err(bad_request("还款账户必须使用相同货币"));
    }
    let data = Snapshot::load(&tx).await?;
    match form.mode.as_str() {
        "custom" | "minimum" => {
            let detail = account_detail::Entity::find_by_id(to.id)
                .one(&tx)
                .await
                .map_err(err500)?;
            let ordinary = data.ordinary(&to, detail.as_ref(), &dek, time_zone)?;
            let amount = amount(&form.amount)?;
            let allow_overpayment = to.kind == "credit_card" && form.mode == "custom";
            if !allow_overpayment && amount > ordinary {
                return Err(bad_request("还款不能超过普通欠款；分期请单独选择还款"));
            }
            let plan = if form.mode == "minimum" {
                if happened_at > chrono::Utc::now().naive_utc() {
                    return Err(bad_request("最低还款转分期不能使用未来时间"));
                }
                snapshot(&form.ordinary_cents, ordinary)?;
                if amount >= ordinary {
                    return Err(bad_request("最低还款必须小于普通欠款"));
                }
                Some(super::installments::parse_input(
                    &form.installment_term,
                    &form.installment_method,
                    &form.installment_annual_rate,
                    &form.installment_fee,
                    &form.installment_first_due_date,
                )?)
            } else {
                None
            };
            let from_balance = super::accounts::current_balance_on(&tx, &dek, from.id).await?;
            let to_balance = super::accounts::current_balance_on(&tx, &dek, to.id).await?;
            if !allow_overpayment {
                let actual_debt = to_balance
                    .checked_neg()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| bad_request("该信用账户当前没有实际欠款"))?;
                if amount > actual_debt {
                    return Err(bad_request("还款不能超过实际欠款"));
                }
                if plan.is_some() && ordinary > actual_debt {
                    return Err(bad_request(
                        "普通欠款估算高于实际欠款，不能创建分期；请先核对未来日期流水、退款及余额调整",
                    ));
                }
            }
            let projected_from = from_balance
                .checked_sub(amount)
                .ok_or_else(|| bad_request("余额超出范围"))?;
            if projected_from < 0 {
                return Err(bad_request("还款来源余额不足"));
            }
            super::accounts::ensure_allowed_balance(&tx, &dek, from.id, projected_from).await?;
            super::accounts::ensure_allowed_balance(&tx, &dek, to.id, add(to_balance, amount)?)
                .await?;
            let payment = transfer::ActiveModel {
                from_account_id: Set(from.id),
                to_account_id: Set(to.id),
                amount: Set(crypto::encrypt_cents(&dek, amount)),
                to_amount: Set(crypto::encrypt_cents(&dek, amount)),
                fee_rate_bps: Set(crypto::encrypt_cents(&dek, 0)),
                fee_amount: Set(crypto::encrypt_cents(&dek, 0)),
                fee_bill_id: Set(None),
                note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
                happened_at: Set(happened_at),
                created_at: Set(chrono::Utc::now()),
                ..Default::default()
            }
            .insert(&tx)
            .await
            .map_err(err500)?;
            if let Some(plan) = plan {
                let principal = ordinary
                    .checked_sub(amount)
                    .ok_or_else(|| bad_request("分期本金超出范围"))?;
                super::installments::create_statement_plan(
                    &tx,
                    &dek,
                    to.id,
                    principal,
                    payment.id,
                    happened_at,
                    plan,
                )
                .await?;
            }
        }
        "installment_next" | "installment_all" => {
            let plan = form
                .plan_id
                .and_then(|id| data.plans.get(&id))
                .ok_or_else(|| bad_request("请选择分期计划"))?;
            if plan.deleted_at.is_some() {
                return Err(bad_request("分期已删除，不能继续还款"));
            }
            if plan.account_id != to.id {
                return Err(bad_request("分期计划不属于还款目标"));
            }
            let refund = RefundContext::new(&dek, &data.bills)
                .and_then(|context| context.state(&dek, plan))
                .map_err(|error| bad_request(&error))?;
            if refund.full_refund || refund.needs_review {
                return Err(bad_request("分期已退款终止或需要手动核对"));
            }
            let mut selected: Vec<_> = data
                .items
                .iter()
                .filter(|item| item.plan_id == plan.id && item.paid_at.is_none())
                .cloned()
                .collect();
            let next = selected
                .first()
                .ok_or_else(|| bad_request("分期已全部还清"))?;
            if form.mode == "installment_next" {
                if form.item_id != Some(next.id) {
                    return Err(bad_request("本期计划已变化，请刷新后重新确认"));
                }
                selected.truncate(1);
            } else {
                let remaining = selected.iter().try_fold(0, |total, item| {
                    add(total, crypto::decrypt_cents(&dek, &item.total))
                })?;
                snapshot(&form.remaining_cents, remaining)?;
            }
            super::installments::pay_items(&tx, &dek, plan, &selected, from.id, happened_at)
                .await?;
        }
        _ => return Err(bad_request("还款方式无效")),
    }
    tx.commit().await.map_err(err500)?;
    let destination = redirect(form.redirect_to.as_deref());
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
    {
        Ok(Json(CreateResponse {
            ok: true,
            message: "还款已记录",
            redirect: destination.into(),
        })
        .into_response())
    } else {
        Ok(Redirect::to(destination).into_response())
    }
}

#[derive(Default, Deserialize)]
pub struct NewQuery {
    #[serde(default)]
    account_id: Option<i64>,
}
struct AccountOption {
    id: i64,
    name: String,
}
#[derive(Template)]
#[template(path = "repayment_form.html")]
struct NewTemplate {
    accounts: Vec<AccountOption>,
    sources: Vec<AccountOption>,
    selected_id: i64,
    currency: String,
    allow_overpayment: bool,
    ordinary_cents: String,
    ordinary_amount: String,
    plans: Vec<PlanOption>,
    happened_at: String,
    first_due_date: String,
}
pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Query(query): Query<NewQuery>,
) -> HandlerResult<Html<String>> {
    let tx = state.db.begin().await.map_err(err500)?;
    let all = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&tx)
        .await
        .map_err(err500)?;
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&tx)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let mut names = super::bills::account_display_names(&dek, &all, &details);
    let accounts = all
        .iter()
        .filter(|account| credit(account))
        .map(|account| AccountOption {
            id: account.id,
            name: names.remove(&account.id).unwrap_or_default(),
        })
        .collect();
    let selected = match query.account_id {
        Some(id) => Some(
            all.iter()
                .find(|account| account.id == id && credit(account))
                .ok_or_else(|| bad_request("请选择信用账户"))?,
        ),
        None => all.iter().find(|account| credit(account)),
    };
    let mut page = NewTemplate {
        accounts,
        sources: Vec::new(),
        selected_id: 0,
        currency: String::new(),
        allow_overpayment: false,
        ordinary_cents: "0".into(),
        ordinary_amount: "0.00".into(),
        plans: Vec::new(),
        happened_at: chrono::Utc::now().format(TIME_FMT).to_string(),
        first_due_date: (time_zone.today() + chrono::Months::new(1)).to_string(),
    };
    if let Some(target) = selected {
        page.selected_id = target.id;
        page.sources = all
            .iter()
            .filter(|account| {
                !credit(account)
                    && crate::investment_funds::is_money_account(account)
                    && account.currency == target.currency
            })
            .map(|account| AccountOption {
                id: account.id,
                name: names.remove(&account.id).unwrap_or_default(),
            })
            .collect();
        let data = Snapshot::load(&tx).await?;
        let ordinary = data.ordinary(target, details.get(&target.id), &dek, time_zone)?;
        let options = data.options(target, ordinary, &dek)?;
        page.currency = options.currency;
        page.allow_overpayment = options.allow_overpayment;
        page.ordinary_cents = options.ordinary_cents;
        page.ordinary_amount = options.ordinary_amount;
        page.plans = options.plans;
    }
    tx.commit().await.map_err(err500)?;
    Ok(Html(page.render().map_err(err500)?))
}

pub async fn cancel_conversion(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let tx = state.db.begin().await.map_err(err500)?;
    let plan = installment_plan::Entity::find_by_id(id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期计划不存在".into()))?;
    if plan.deleted_at.is_some() {
        return Err(bad_request("已删除分期不能撤销转换"));
    }
    if plan.bill_id.is_some() {
        return Err(bad_request("只有账单剩余欠款转换可以撤销"));
    }
    if installment_item::Entity::find()
        .filter(installment_item::Column::PlanId.eq(id))
        .filter(installment_item::Column::PaidAt.is_not_null())
        .one(&tx)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request("已有真实还款，不能撤销账单分期转换"));
    }
    installment_item::Entity::delete_many()
        .filter(installment_item::Column::PlanId.eq(id))
        .exec(&tx)
        .await
        .map_err(err500)?;
    installment_plan::Entity::delete_by_id(id)
        .exec(&tx)
        .await
        .map_err(err500)?;
    tx.commit().await.map_err(err500)?;
    Ok(Redirect::to(&format!(
        "/repayments/new?account_id={}",
        plan.account_id
    )))
}
