use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::{Datelike, Months, NaiveDate};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
};

use crate::{
    crypto, currency,
    entity::{
        account, account_detail, bill, category, installment_item, installment_plan, transfer,
    },
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

fn bad_state(message: String) -> (StatusCode, String) {
    bad_request(&message)
}

fn plan_principal(
    dek: &crypto::Dek,
    plan: &installment_plan::Model,
    bill: Option<&bill::Model>,
) -> HandlerResult<i64> {
    if !plan.principal.is_empty() {
        return Ok(crypto::decrypt_cents(dek, &plan.principal));
    }
    bill.map(|bill| crypto::decrypt_cents(dek, &bill.amount))
        .ok_or_else(|| bad_request("分期本金不存在"))
}

fn is_returned_plan(
    plan: &installment_plan::Model,
    refund: &crate::installment_state::RefundState,
) -> bool {
    plan.deleted_at.is_none() && plan.bill_id.is_some() && refund.refunded > 0
}

struct ReviewRow {
    id: i64,
    sequence: i32,
    due_date: String,
    principal: String,
    interest: String,
    fee: String,
}

pub(crate) async fn pay_items(
    tx: &sea_orm::DatabaseTransaction,
    dek: &crypto::Dek,
    plan: &installment_plan::Model,
    items: &[installment_item::Model],
    repayment_account_id: i64,
    happened_at: chrono::NaiveDateTime,
) -> HandlerResult<()> {
    let current_plan = installment_plan::Entity::find_by_id(plan.id)
        .one(tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("分期计划不存在"))?;
    if current_plan.deleted_at.is_some() {
        return Err(bad_request("分期已删除，不能继续还款"));
    }
    let bills = bill::Entity::find()
        .filter(bill::Column::AccountId.eq(current_plan.account_id))
        .all(tx)
        .await
        .map_err(err500)?;
    let refund = crate::installment_state::RefundContext::new(dek, &bills)
        .map_err(bad_state)?
        .state(dek, &current_plan)
        .map_err(bad_state)?;
    if refund.full_refund || refund.needs_review {
        return Err(bad_request("分期已退款终止或需要核对剩余计划，不能还款"));
    }
    if items.is_empty() {
        return Err(bad_request("没有可还期次"));
    }
    let credit = account::Entity::find_by_id(current_plan.account_id)
        .one(tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("信用账户不存在"))?;
    if !matches!(credit.kind.as_str(), "credit_card" | "credit_service") {
        return Err(bad_request("目标必须是信用账户"));
    }
    let source = account::Entity::find_by_id(repayment_account_id)
        .one(tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("还款账户不存在"))?;
    if matches!(source.kind.as_str(), "credit_card" | "credit_service")
        || source.currency != credit.currency
    {
        return Err(bad_request("还款来源必须是同币种非信用账户"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut current = Vec::new();
    let mut principal_total = 0;
    let mut debit_total = 0;
    for requested in items {
        if !seen.insert(requested.id) {
            return Err(bad_request("重复还款期次"));
        }
        let item = installment_item::Entity::find_by_id(requested.id)
            .one(tx)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("还款期次不存在"))?;
        if item.plan_id != current_plan.id
            || item.paid_at.is_some()
            || item.principal != requested.principal
            || item.interest != requested.interest
            || item.fee != requested.fee
            || item.due_date != requested.due_date
        {
            return Err(bad_request("期次已还款或计划已变更，请刷新"));
        }
        let principal = crypto::decrypt_cents(dek, &item.principal);
        let interest = crypto::decrypt_cents(dek, &item.interest);
        let fee = crypto::decrypt_cents(dek, &item.fee);
        if principal < 0 || interest < 0 || fee < 0 {
            return Err(bad_request("分期金额不能为负数"));
        }
        let charges = checked_add(interest, fee)?;
        principal_total = checked_add(principal_total, principal)?;
        debit_total = checked_add(debit_total, checked_add(principal, charges)?)?;
        current.push((item, principal, charges));
    }
    super::accounts::ensure_balance_delta_on(
        tx,
        dek,
        source.id,
        debit_total
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?,
    )
    .await?;
    super::accounts::ensure_balance_delta_on(tx, dek, credit.id, principal_total).await?;
    let original = current_plan
        .bill_id
        .and_then(|id| bills.iter().find(|bill| bill.id == id));
    let title = original
        .map(|bill| bill_title(dek, bill))
        .unwrap_or_else(|| "最低还款余款分期".into());
    let category_id = if current.iter().any(|(_, _, charges)| *charges > 0) {
        let categories = category::Entity::find()
            .filter(category::Column::Kind.eq("expense"))
            .all(tx)
            .await
            .map_err(err500)?;
        Some(
            if let Some(category) = categories
                .iter()
                .find(|item| crypto::decrypt_string(dek, &item.name) == "分期费用")
            {
                category.id
            } else {
                category::ActiveModel {
                    kind: Set("expense".into()),
                    name: Set(crypto::encrypt(dek, "分期费用".as_bytes())),
                    is_food: Set(false),
                    count_limit: Set(String::new()),
                    count_limit_period: Set("month".into()),
                    created_at: Set(chrono::Utc::now()),
                    ..Default::default()
                }
                .insert(tx)
                .await
                .map_err(err500)?
                .id
            },
        )
    } else {
        None
    };
    for (item, principal, charges) in current {
        let principal_transfer_id = if principal > 0 {
            Some(
                transfer::ActiveModel {
                    from_account_id: Set(source.id),
                    to_account_id: Set(credit.id),
                    amount: Set(crypto::encrypt_cents(dek, principal)),
                    to_amount: Set(crypto::encrypt_cents(dek, principal)),
                    note: Set(crypto::encrypt(
                        dek,
                        format!("分期还款：{title} · 第 {} 期本金", item.sequence).as_bytes(),
                    )),
                    happened_at: Set(happened_at),
                    created_at: Set(chrono::Utc::now()),
                    ..Default::default()
                }
                .insert(tx)
                .await
                .map_err(err500)?
                .id,
            )
        } else {
            None
        };
        let charge_bill_id = if charges > 0 {
            Some(
                bill::ActiveModel {
                    account_id: Set(source.id),
                    kind: Set("expense".into()),
                    amount: Set(crypto::encrypt_cents(dek, charges)),
                    category: Set(crypto::encrypt(dek, "分期费用".as_bytes())),
                    category_id: Set(category_id),
                    is_food: Set(false),
                    is_private: Set(original.is_some_and(|bill| bill.is_private)),
                    note: Set(crypto::encrypt(
                        dek,
                        format!("分期还款：{title} · 第 {} 期利息与手续费", item.sequence)
                            .as_bytes(),
                    )),
                    happened_at: Set(happened_at),
                    created_at: Set(chrono::Utc::now()),
                    ..Default::default()
                }
                .insert(tx)
                .await
                .map_err(err500)?
                .id,
            )
        } else {
            None
        };
        let mut active = item.into_active_model();
        active.paid_at = Set(Some(happened_at.and_utc()));
        active.repayment_account_id = Set(Some(source.id));
        active.principal_transfer_id = Set(principal_transfer_id);
        active.charge_bill_id = Set(charge_bill_id);
        active.update(tx).await.map_err(err500)?;
    }
    Ok(())
}

struct ReviewForm {
    refunded_cents: String,
    schedule: String,
    additional_schedule: String,
    rows: HashMap<String, String>,
}

#[derive(Deserialize)]
struct ReviewedItem {
    id: i64,
    principal: String,
    interest: String,
    fee: String,
    due_date: String,
}

fn review_cents(value: &str) -> HandlerResult<i64> {
    let value = Decimal::from_str(value.trim()).map_err(|_| bad_request("金额格式不正确"))?;
    let cents = value
        .checked_mul(Decimal::from(100))
        .ok_or_else(|| bad_request("金额超出范围"))?;
    if cents.is_sign_negative() || cents.fract() != Decimal::ZERO {
        return Err(bad_request("金额必须非负且最多两位小数"));
    }
    cents.to_i64().ok_or_else(|| bad_request("金额超出范围"))
}

pub async fn review(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> HandlerResult<Redirect> {
    let mut rows = HashMap::new();
    for (key, value) in fields {
        if rows.insert(key, value).is_some() {
            return Err(bad_request("不能重复提交期次字段"));
        }
    }
    let form = ReviewForm {
        refunded_cents: rows
            .remove("refunded_cents")
            .ok_or_else(|| bad_request("缺少退款基准"))?,
        schedule: rows.remove("schedule").unwrap_or_default(),
        additional_schedule: rows.remove("additional_schedule").unwrap_or_default(),
        rows,
    };
    let _guard = state.balance_writes.lock().await;
    let tx = state.db.begin().await.map_err(err500)?;
    let plan = installment_plan::Entity::find_by_id(id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("分期不存在"))?;
    if plan.deleted_at.is_some() {
        return Err(bad_request("分期已删除，不能核对或恢复计划"));
    }
    let bills = bill::Entity::find()
        .filter(bill::Column::AccountId.eq(plan.account_id))
        .all(&tx)
        .await
        .map_err(err500)?;
    let refund = crate::installment_state::RefundContext::new(&dek, &bills)
        .map_err(bad_state)?
        .state(&dek, &plan)
        .map_err(bad_state)?;
    let baseline = form
        .refunded_cents
        .parse::<i64>()
        .map_err(|_| bad_request("退款基准不正确"))?;
    if !refund.needs_review || refund.full_refund || baseline != refund.refunded {
        return Err(bad_request("退款已变更或计划已核对，请刷新"));
    }
    let submitted: Vec<ReviewedItem> = if !form.schedule.is_empty() {
        serde_json::from_str(&form.schedule).map_err(|_| bad_request("核对计划格式不正确"))?
    } else {
        let mut submitted = Vec::new();
        for key in form.rows.keys().filter(|key| key.starts_with("principal_")) {
            let id = key
                .trim_start_matches("principal_")
                .parse::<i64>()
                .map_err(|_| bad_request("期次编号不正确"))?;
            let field = |name: &str| {
                form.rows
                    .get(&format!("{name}_{id}"))
                    .cloned()
                    .ok_or_else(|| bad_request("期次字段不完整"))
            };
            submitted.push(ReviewedItem {
                id,
                principal: field("principal")?,
                interest: field("interest")?,
                fee: field("fee")?,
                due_date: field("due_date")?,
            });
        }
        submitted
    };
    let items = installment_item::Entity::find()
        .filter(installment_item::Column::PlanId.eq(id))
        .all(&tx)
        .await
        .map_err(err500)?;
    let unpaid = items
        .iter()
        .filter(|item| item.paid_at.is_none())
        .collect::<Vec<_>>();
    if submitted.len() != unpaid.len() {
        return Err(bad_request("必须提交全部未还期次"));
    }
    let original = plan
        .bill_id
        .and_then(|id| bills.iter().find(|bill| bill.id == id));
    let mut paid = 0;
    for item in items.iter().filter(|item| item.paid_at.is_some()) {
        paid = checked_add(paid, crypto::decrypt_cents(&dek, &item.principal))?;
    }
    let expected = plan_principal(&dek, &plan, original)?
        .checked_sub(refund.refunded)
        .and_then(|value| value.checked_sub(paid))
        .ok_or_else(|| bad_request("金额超出范围"))?
        .max(0);
    let mut seen = std::collections::HashSet::new();
    let mut sum = 0;
    let mut updates = Vec::new();
    let mut additions = Vec::new();
    let mut sequence = items.iter().map(|item| item.sequence).max().unwrap_or(0);
    for row in submitted {
        if !seen.insert(row.id) {
            return Err(bad_request("不能重复提交期次"));
        }
        let item = unpaid
            .iter()
            .find(|item| item.id == row.id)
            .ok_or_else(|| bad_request("只能修改本计划未还期次"))?;
        let principal = review_cents(&row.principal)?;
        let interest = review_cents(&row.interest)?;
        let fee = review_cents(&row.fee)?;
        let due_date = NaiveDate::parse_from_str(&row.due_date, "%Y-%m-%d")
            .map_err(|_| bad_request("还款日期不正确"))?;
        let total = checked_add(principal, checked_add(interest, fee)?)?;
        sum = checked_add(sum, principal)?;
        let mut active = (**item).clone().into_active_model();
        active.principal = Set(crypto::encrypt_cents(&dek, principal));
        active.interest = Set(crypto::encrypt_cents(&dek, interest));
        active.fee = Set(crypto::encrypt_cents(&dek, fee));
        active.total = Set(crypto::encrypt_cents(&dek, total));
        active.due_date = Set(due_date);
        updates.push(active);
    }
    for line in form
        .additional_schedule
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if unpaid.len() + additions.len() >= 360 {
            return Err(bad_request("核对后最多保留 360 个未还期次"));
        }
        let mut fields = line.split(',').map(str::trim);
        let due_date = NaiveDate::parse_from_str(fields.next().unwrap_or_default(), "%Y-%m-%d")
            .map_err(|_| bad_request("新增期次每行须为：还款日,本金,利息,手续费"))?;
        let principal = review_cents(fields.next().unwrap_or_default())?;
        let interest = review_cents(fields.next().unwrap_or_default())?;
        let fee = review_cents(fields.next().unwrap_or_default())?;
        if fields.next().is_some() {
            return Err(bad_request(
                "新增期次每行只能填写还款日、本金、利息和手续费",
            ));
        }
        let total = checked_add(principal, checked_add(interest, fee)?)?;
        if total == 0 {
            return Err(bad_request("新增期次必须有实际待还金额"));
        }
        sum = checked_add(sum, principal)?;
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| bad_request("期次编号超出范围"))?;
        additions.push(installment_item::ActiveModel {
            plan_id: Set(id),
            sequence: Set(sequence),
            due_date: Set(due_date),
            principal: Set(crypto::encrypt_cents(&dek, principal)),
            interest: Set(crypto::encrypt_cents(&dek, interest)),
            fee: Set(crypto::encrypt_cents(&dek, fee)),
            total: Set(crypto::encrypt_cents(&dek, total)),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        });
    }
    if sum != expected {
        return Err(bad_request(
            "未还本金合计必须等于原本金减累计退款及已还本金",
        ));
    }
    for active in updates {
        active.update(&tx).await.map_err(err500)?;
    }
    for active in additions {
        active.insert(&tx).await.map_err(err500)?;
    }
    let mut active = plan.into_active_model();
    active.refund_reviewed_amount = Set(crypto::encrypt_cents(&dek, refund.refunded));
    active.update(&tx).await.map_err(err500)?;
    tx.commit().await.map_err(err500)?;
    Ok(Redirect::to(&format!("/installments/{id}")))
}

fn bill_title(dek: &crypto::Dek, bill: &bill::Model) -> String {
    if bill.is_private {
        return "隐私账单".into();
    }
    let category = crypto::decrypt_string(dek, &bill.category);
    let note = crypto::decrypt_string(dek, &bill.note);
    if note.is_empty() {
        category
    } else {
        format!("{category} · {note}")
    }
}

pub(crate) struct NewPlan {
    pub term_months: i32,
    pub method: String,
    pub annual_rate_bps: i64,
    pub fee: i64,
    pub first_due_date: NaiveDate,
}

struct ScheduleAmount {
    sequence: i32,
    due_date: NaiveDate,
    principal: i64,
    interest: i64,
    fee: i64,
    total: i64,
}

struct InstallmentRow {
    id: i64,
    title: String,
    account_name: String,
    principal: String,
    term: String,
    method: String,
    annual_rate: String,
    total_repayment: String,
    remaining: String,
    currency: String,
    annual_amounts: AnnualAmounts,
    annual_repayment: String,
    annual_unpaid: String,
    next_due: String,
    progress: String,
    overdue_count: usize,
    method_code: String,
    next_due_date: Option<NaiveDate>,
    completed: bool,
    refunded: bool,
    needs_review: bool,
    returned: bool,
    deleted: bool,
    can_delete: bool,
}

struct ScheduleRow {
    id: i64,
    sequence: i32,
    due_date: String,
    principal: String,
    interest: String,
    fee: String,
    total: String,
    paid: bool,
    overdue: bool,
    repayment_account_name: String,
}

struct RepaymentAccountOption {
    id: i64,
    name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AnnualAmounts {
    total: i64,
    unpaid: i64,
}

impl AnnualAmounts {
    fn add(&mut self, total: i64, unpaid: i64) -> HandlerResult<()> {
        let total = checked_add(self.total, total)?;
        let unpaid = checked_add(self.unpaid, unpaid)?;
        *self = Self { total, unpaid };
        Ok(())
    }

    fn paid(self) -> HandlerResult<i64> {
        self.total
            .checked_sub(self.unpaid)
            .ok_or_else(|| bad_request("分期金额超出范围"))
    }
}

struct CurrencyAnnualRow {
    currency: String,
    total: String,
    unpaid: String,
}

struct AnnualRow {
    year: i32,
    total: String,
    paid: String,
    unpaid: String,
}

fn annual_row(year: i32, amounts: AnnualAmounts, currency: &str) -> HandlerResult<AnnualRow> {
    Ok(AnnualRow {
        year,
        total: currency::format(amounts.total, currency),
        paid: currency::format(amounts.paid()?, currency),
        unpaid: currency::format(amounts.unpaid, currency),
    })
}

fn annual_repayments<'a>(
    dek: &crypto::Dek,
    items: impl IntoIterator<Item = &'a installment_item::Model>,
) -> HandlerResult<BTreeMap<i32, AnnualAmounts>> {
    let mut years = BTreeMap::<i32, AnnualAmounts>::new();
    for item in items {
        let total = crypto::decrypt_cents(dek, &item.total);
        years
            .entry(item.due_date.year())
            .or_default()
            .add(total, if item.paid_at.is_none() { total } else { 0 })?;
    }
    Ok(years)
}

#[derive(Template)]
#[template(path = "installments.html")]
struct InstallmentsTemplate {
    page_heading: String,
    advanced_search: bool,
    search_action: String,
    plans: Vec<InstallmentRow>,
    current_year: i32,
    annual_summary: Vec<CurrencyAnnualRow>,
    filtered_count: usize,
    returned_count: usize,
    mode: String,
    keyword: String,
    status: String,
    method: String,
    start_date: String,
    end_date: String,
    has_filters: bool,
    per_page: usize,
    pagination: super::PaginationView,
}

#[derive(Template)]
#[template(path = "installment_detail.html")]
struct InstallmentDetailTemplate {
    title: String,
    bill_id: Option<i64>,
    refunded: bool,
    full_refund: bool,
    deleted: bool,
    needs_review: bool,
    refunded_cents: i64,
    review_schedule: Vec<ReviewRow>,
    account_name: String,
    currency: String,
    principal: String,
    term: String,
    method: String,
    annual_rate: String,
    total_interest: String,
    total_fee: String,
    total_repayment: String,
    first_due_date: String,
    schedule: Vec<ScheduleRow>,
    current_annual: AnnualRow,
    annual_schedule: Vec<AnnualRow>,
    repayment_accounts: Vec<RepaymentAccountOption>,
    plan_id: i64,
    per_page: usize,
    total_records: usize,
    pagination: super::PaginationView,
}

#[derive(Deserialize)]
pub struct PaidFormData {
    paid: bool,
    #[serde(default)]
    repayment_account_id: Option<i64>,
}

#[derive(Default, Deserialize)]
pub struct InstallmentsQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
}

#[derive(Default, Deserialize)]
pub struct InstallmentDetailQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
}

pub(crate) fn parse_input(
    term: &str,
    method: &str,
    annual_rate: &str,
    fee: &str,
    first_due_date: &str,
) -> HandlerResult<NewPlan> {
    let term_months: i32 = term
        .trim()
        .parse()
        .map_err(|_| bad_request("分期期限无效"))?;
    if !matches!(
        term_months,
        3 | 6 | 9 | 12 | 24 | 36 | 60 | 120 | 180 | 240 | 360
    ) {
        return Err(bad_request("分期期限无效"));
    }
    if !matches!(method, "equal_payment" | "equal_principal" | "flat") {
        return Err(bad_request("还款方式无效"));
    }
    let rate = Decimal::from_str(annual_rate.trim())
        .map_err(|_| bad_request("年利率格式不正确"))?
        .round_dp(2);
    if rate < Decimal::ZERO || rate > Decimal::from(100) {
        return Err(bad_request("年利率必须在 0% 到 100% 之间"));
    }
    let annual_rate_bps = (rate * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("年利率超出范围"))?;
    let fee_decimal = Decimal::from_str(fee.trim())
        .map_err(|_| bad_request("总手续费格式不正确"))?
        .round_dp(2);
    if fee_decimal < Decimal::ZERO {
        return Err(bad_request("总手续费不能小于 0"));
    }
    let fee = (fee_decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("总手续费超出范围"))?;
    let first_due_date = NaiveDate::parse_from_str(first_due_date.trim(), "%Y-%m-%d")
        .map_err(|_| bad_request("首期还款日格式不正确"))?;
    Ok(NewPlan {
        term_months,
        method: method.into(),
        annual_rate_bps,
        fee,
        first_due_date,
    })
}

fn checked_add(left: i64, right: i64) -> HandlerResult<i64> {
    left.checked_add(right)
        .ok_or_else(|| bad_request("分期金额超出范围"))
}

fn split_amount(total: i64, count: i32, index: i32) -> i64 {
    let base = total / i64::from(count);
    let remainder = total % i64::from(count);
    base + i64::from(index <= remainder as i32)
}

fn schedule(principal: i64, plan: &NewPlan) -> HandlerResult<Vec<ScheduleAmount>> {
    let count = plan.term_months;
    let monthly_rate = Decimal::from(plan.annual_rate_bps) / Decimal::from(10_000 * 12);
    let fixed_payment = if monthly_rate.is_zero() {
        Decimal::from(principal) / Decimal::from(count)
    } else {
        let mut factor = Decimal::ONE;
        for _ in 0..count {
            factor *= Decimal::ONE + monthly_rate;
        }
        Decimal::from(principal) * monthly_rate * factor / (factor - Decimal::ONE)
    };
    let flat_total_interest =
        (Decimal::from(principal) * Decimal::from(plan.annual_rate_bps) * Decimal::from(count)
            / Decimal::from(10_000 * 12))
        .round()
        .to_i64()
        .ok_or_else(|| bad_request("利息金额超出范围"))?;

    let mut rows = Vec::with_capacity(count as usize);
    let mut remaining = principal;
    for sequence in 1..=count {
        let due_date = plan
            .first_due_date
            .checked_add_months(Months::new((sequence - 1) as u32))
            .ok_or_else(|| bad_request("还款日期超出范围"))?;
        let (principal_part, interest) = match plan.method.as_str() {
            "equal_principal" => {
                let principal_part = split_amount(principal, count, sequence).min(remaining);
                let interest = (Decimal::from(remaining) * monthly_rate)
                    .round()
                    .to_i64()
                    .ok_or_else(|| bad_request("利息金额超出范围"))?;
                (principal_part, interest)
            }
            "flat" => (
                split_amount(principal, count, sequence).min(remaining),
                split_amount(flat_total_interest, count, sequence),
            ),
            _ => {
                let interest = (Decimal::from(remaining) * monthly_rate)
                    .round()
                    .to_i64()
                    .ok_or_else(|| bad_request("利息金额超出范围"))?;
                let principal_part = if sequence == count {
                    remaining
                } else {
                    (fixed_payment.round().to_i64().unwrap_or_default() - interest)
                        .max(0)
                        .min(remaining)
                };
                (principal_part, interest)
            }
        };
        remaining = remaining
            .checked_sub(principal_part)
            .ok_or_else(|| bad_request("分期本金超出范围"))?;
        let fee = split_amount(plan.fee, count, sequence);
        let total = checked_add(checked_add(principal_part, interest)?, fee)?;
        rows.push(ScheduleAmount {
            sequence,
            due_date,
            principal: principal_part,
            interest,
            fee,
            total,
        });
    }
    if remaining != 0 {
        return Err(bad_request("无法生成完整的分期本金计划"));
    }
    Ok(rows)
}

pub(crate) async fn create_plan<C: ConnectionTrait>(
    db: &C,
    dek: &crypto::Dek,
    bill_id: i64,
    account_id: i64,
    principal: i64,
    input: NewPlan,
) -> HandlerResult<i64> {
    let amounts = schedule(principal, &input)?;
    let plan = installment_plan::ActiveModel {
        bill_id: Set(Some(bill_id)),
        principal: Set(crypto::encrypt_cents(dek, principal)),
        account_id: Set(account_id),
        term_months: Set(input.term_months),
        method: Set(input.method),
        annual_rate_bps: Set(crypto::encrypt_cents(dek, input.annual_rate_bps)),
        fee: Set(crypto::encrypt_cents(dek, input.fee)),
        first_due_date: Set(input.first_due_date),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(err500)?;
    for amount in amounts {
        installment_item::ActiveModel {
            plan_id: Set(plan.id),
            sequence: Set(amount.sequence),
            due_date: Set(amount.due_date),
            principal: Set(crypto::encrypt_cents(dek, amount.principal)),
            interest: Set(crypto::encrypt_cents(dek, amount.interest)),
            fee: Set(crypto::encrypt_cents(dek, amount.fee)),
            total: Set(crypto::encrypt_cents(dek, amount.total)),
            paid_at: Set(None),
            repayment_account_id: Set(None),
            principal_transfer_id: Set(None),
            charge_bill_id: Set(None),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(err500)?;
    }
    Ok(plan.id)
}

pub(crate) async fn create_statement_plan<C: ConnectionTrait>(
    db: &C,
    dek: &crypto::Dek,
    account_id: i64,
    principal: i64,
    repayment_transfer_id: i64,
    converted_at: chrono::NaiveDateTime,
    input: NewPlan,
) -> HandlerResult<i64> {
    let amounts = schedule(principal, &input)?;
    let plan = installment_plan::ActiveModel {
        bill_id: Set(None),
        principal: Set(crypto::encrypt_cents(dek, principal)),
        repayment_transfer_id: Set(Some(repayment_transfer_id)),
        converted_at: Set(Some(converted_at)),
        account_id: Set(account_id),
        term_months: Set(input.term_months),
        method: Set(input.method),
        annual_rate_bps: Set(crypto::encrypt_cents(dek, input.annual_rate_bps)),
        fee: Set(crypto::encrypt_cents(dek, input.fee)),
        first_due_date: Set(input.first_due_date),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(err500)?;
    for amount in amounts {
        installment_item::ActiveModel {
            plan_id: Set(plan.id),
            sequence: Set(amount.sequence),
            due_date: Set(amount.due_date),
            principal: Set(crypto::encrypt_cents(dek, amount.principal)),
            interest: Set(crypto::encrypt_cents(dek, amount.interest)),
            fee: Set(crypto::encrypt_cents(dek, amount.fee)),
            total: Set(crypto::encrypt_cents(dek, amount.total)),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(err500)?;
    }
    Ok(plan.id)
}

fn method_label(method: &str) -> &'static str {
    match method {
        "equal_principal" => "等额本金",
        "flat" => "等本等息",
        _ => "等额本息",
    }
}

fn term_label(months: i32) -> String {
    if months > 24 {
        format!("{} 年（{} 期月供）", months / 12, months)
    } else {
        format!("{months} 期")
    }
}

fn sum_items<'a>(
    dek: &crypto::Dek,
    items: impl IntoIterator<Item = &'a installment_item::Model>,
) -> HandlerResult<(i64, i64, i64)> {
    let mut interest = 0i64;
    let mut fee = 0i64;
    let mut total = 0i64;
    for item in items {
        interest = checked_add(interest, crypto::decrypt_cents(dek, &item.interest))?;
        fee = checked_add(fee, crypto::decrypt_cents(dek, &item.fee))?;
        total = checked_add(total, crypto::decrypt_cents(dek, &item.total))?;
    }
    Ok((interest, fee, total))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Query(query): Query<InstallmentsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, time_zone, query, false).await
}

pub async fn advanced_search(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Query(query): Query<InstallmentsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, time_zone, query, true).await
}

async fn render_list(
    state: &AppState,
    dek: &crypto::Dek,
    time_zone: super::ClientTimeZone,
    mut query: InstallmentsQuery,
    advanced_search: bool,
) -> HandlerResult<Html<String>> {
    if !advanced_search {
        query.mode = "and".into();
        query.status.clear();
        query.method.clear();
        query.start_date.clear();
        query.end_date.clear();
    }
    let start_date = if query.start_date.trim().is_empty() {
        None
    } else {
        Some(
            NaiveDate::parse_from_str(query.start_date.trim(), "%Y-%m-%d")
                .map_err(|_| bad_request("开始日期格式不正确"))?,
        )
    };
    let end_date = if query.end_date.trim().is_empty() {
        None
    } else {
        Some(
            NaiveDate::parse_from_str(query.end_date.trim(), "%Y-%m-%d")
                .map_err(|_| bad_request("结束日期格式不正确"))?,
        )
    };
    if start_date
        .zip(end_date)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(bad_request("开始日期不能晚于结束日期"));
    }
    if !query.status.is_empty()
        && !matches!(
            query.status.as_str(),
            "open"
                | "completed"
                | "overdue"
                | "due_soon"
                | "refunded"
                | "needs_review"
                | "returned"
                | "deleted"
        )
    {
        query.status.clear();
    }
    if !query.method.is_empty()
        && !matches!(
            query.method.as_str(),
            "equal_payment" | "equal_principal" | "flat"
        )
    {
        query.method.clear();
    }
    let plans = installment_plan::Entity::find()
        .order_by_asc(installment_plan::Column::FirstDueDate)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let bills: HashMap<i64, bill::Model> = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| (bill.id, bill))
        .collect();
    let refund_context =
        crate::installment_state::RefundContext::new(dek, bills.values()).map_err(bad_state)?;
    let accounts: HashMap<i64, account::Model> = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|account| (account.id, account))
        .collect();
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let all_items = installment_item::Entity::find()
        .order_by_asc(installment_item::Column::Sequence)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut items_by_plan = HashMap::<i64, Vec<&installment_item::Model>>::new();
    for item in &all_items {
        items_by_plan.entry(item.plan_id).or_default().push(item);
    }
    let today = time_zone.today();
    let current_year = today.year();
    let soon = today + chrono::Duration::days(7);
    let mut rows = Vec::new();
    let mut returned_count = 0usize;
    for plan in plans {
        let bill = plan.bill_id.and_then(|id| bills.get(&id));
        let refund = refund_context.state(dek, &plan).map_err(bad_state)?;
        let deleted = plan.deleted_at.is_some();
        let returned = is_returned_plan(&plan, &refund);
        if returned {
            returned_count += 1;
        }
        let Some(account) = accounts.get(&plan.account_id) else {
            continue;
        };
        let items = items_by_plan
            .get(&plan.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut total_repayment = 0;
        let mut remaining = 0i64;
        let mut annual_amounts = AnnualAmounts::default();
        let mut paid = 0usize;
        let mut overdue_count = 0usize;
        let mut next_due = if deleted {
            "已删除终止".into()
        } else if refund.needs_review {
            "待核对，已暂停".into()
        } else if refund.full_refund {
            "已退款终止".into()
        } else {
            "已全部还清".into()
        };
        let mut next_due_date = None;
        for item in items.iter().filter(|item| {
            item.paid_at.is_some() || (!deleted && !refund.full_refund && !refund.needs_review)
        }) {
            let total = crypto::decrypt_cents(dek, &item.total);
            total_repayment = checked_add(total_repayment, total)?;
            if item.due_date.year() == current_year {
                annual_amounts.add(total, if item.paid_at.is_none() { total } else { 0 })?;
            }
            if item.paid_at.is_some() {
                paid += 1;
                continue;
            }
            remaining = checked_add(remaining, total)?;
            if next_due_date.is_none() {
                next_due_date = Some(item.due_date);
                next_due = format!(
                    "{} · {}",
                    item.due_date.format("%Y-%m-%d"),
                    currency::format(total, &account.currency)
                );
            }
            if item.due_date < today {
                overdue_count += 1;
            }
        }
        let title = bill
            .map(|bill| bill_title(dek, bill))
            .unwrap_or_else(|| "最低还款余款分期".into());
        rows.push(InstallmentRow {
            id: plan.id,
            title,
            account_name: super::bills::account_display_name(
                &dek,
                account,
                details.get(&account.id),
            ),
            principal: currency::format(plan_principal(dek, &plan, bill)?, &account.currency),
            term: if items.len() == plan.term_months as usize {
                term_label(plan.term_months)
            } else {
                format!(
                    "原{} · 核对后共 {} 期",
                    term_label(plan.term_months),
                    items.len()
                )
            },
            method: method_label(&plan.method).into(),
            annual_rate: format!(
                "{}%",
                Decimal::new(crypto::decrypt_cents(&dek, &plan.annual_rate_bps), 2)
            ),
            total_repayment: currency::format(total_repayment, &account.currency),
            remaining: if !deleted && refund.needs_review {
                "待核对".into()
            } else {
                currency::format(remaining, &account.currency)
            },
            currency: account.currency.clone(),
            annual_amounts,
            annual_repayment: currency::format(annual_amounts.total, &account.currency),
            annual_unpaid: if !deleted && refund.needs_review {
                "待核对".into()
            } else {
                currency::format(annual_amounts.unpaid, &account.currency)
            },
            next_due,
            progress: format!("{paid}/{}", items.len()),
            overdue_count,
            method_code: plan.method,
            next_due_date,
            completed: !deleted && paid == items.len(),
            refunded: !deleted && refund.full_refund,
            needs_review: !deleted && refund.needs_review,
            returned,
            deleted,
            can_delete: !deleted && plan.bill_id.is_some(),
        });
    }
    let keyword = query.keyword.trim().to_lowercase();
    let mode_or = query.mode == "or";
    let has_filters = !keyword.is_empty()
        || !query.status.is_empty()
        || !query.method.is_empty()
        || start_date.is_some()
        || end_date.is_some();
    let rows = rows
        .into_iter()
        .filter(|row| {
            if row.deleted && query.status != "deleted" {
                return false;
            }
            if row.refunded && !matches!(query.status.as_str(), "refunded" | "returned") {
                return false;
            }
            let mut conditions = Vec::new();
            if !keyword.is_empty() {
                conditions.push(
                    format!(
                        "{} {} {} {} {}",
                        row.title, row.account_name, row.method, row.term, row.annual_rate
                    )
                    .to_lowercase()
                    .contains(&keyword),
                );
            }
            if !query.status.is_empty() {
                conditions.push(match query.status.as_str() {
                    "deleted" => row.deleted,
                    "returned" => row.returned,
                    "refunded" => row.refunded,
                    "needs_review" => row.needs_review,
                    "completed" => row.completed,
                    "overdue" => row.overdue_count > 0,
                    "due_soon" => row
                        .next_due_date
                        .is_some_and(|date| date >= today && date <= soon),
                    "open" => !row.completed && !row.refunded && !row.deleted,
                    _ => true,
                });
            }
            if !query.method.is_empty() {
                conditions.push(row.method_code == query.method);
            }
            if start_date.is_some() || end_date.is_some() {
                conditions.push(row.next_due_date.is_some_and(|date| {
                    start_date.is_none_or(|start| date >= start)
                        && end_date.is_none_or(|end| date <= end)
                }));
            }
            if conditions.is_empty() {
                true
            } else if mode_or {
                conditions.into_iter().any(|matched| matched)
            } else {
                conditions.into_iter().all(|matched| matched)
            }
        })
        .collect::<Vec<_>>();
    let mut annual_by_currency = BTreeMap::<&str, AnnualAmounts>::new();
    for row in &rows {
        annual_by_currency
            .entry(&row.currency)
            .or_default()
            .add(row.annual_amounts.total, row.annual_amounts.unpaid)?;
    }
    let annual_summary = annual_by_currency
        .into_iter()
        .map(|(currency, amounts)| CurrencyAnnualRow {
            currency: currency.to_string(),
            total: currency::format(amounts.total, currency),
            unpaid: currency::format(amounts.unpaid, currency),
        })
        .collect();
    let total_records = rows.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let rows = rows
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();
    Ok(Html(
        InstallmentsTemplate {
            page_heading: if advanced_search {
                "分期高级搜索"
            } else {
                "分期"
            }
            .into(),
            advanced_search,
            search_action: if advanced_search {
                "/installments/search"
            } else {
                "/installments"
            }
            .into(),
            plans: rows,
            current_year,
            annual_summary,
            filtered_count: total_records,
            returned_count,
            mode: if mode_or { "or" } else { "and" }.into(),
            keyword: query.keyword.clone(),
            status: query.status.clone(),
            method: query.method.clone(),
            start_date: query.start_date.clone(),
            end_date: query.end_date.clone(),
            has_filters,
            per_page: pagination.per_page,
            pagination: super::pagination_view(
                &pagination,
                total_records,
                if advanced_search {
                    "/installments/search"
                } else {
                    "/installments"
                },
                "个分期",
                [
                    ("mode", query.mode.clone()),
                    ("keyword", query.keyword.clone()),
                    ("status", query.status.clone()),
                    ("method", query.method.clone()),
                    ("start_date", query.start_date.clone()),
                    ("end_date", query.end_date.clone()),
                ],
            ),
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn detail(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Path(id): Path<i64>,
    Query(query): Query<InstallmentDetailQuery>,
) -> HandlerResult<Html<String>> {
    let plan = installment_plan::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期计划不存在".into()))?;
    let bills = bill::Entity::find()
        .filter(bill::Column::AccountId.eq(plan.account_id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let bill = plan
        .bill_id
        .and_then(|id| bills.iter().find(|bill| bill.id == id));
    let refund = crate::installment_state::RefundContext::new(&dek, &bills)
        .map_err(bad_state)?
        .state(&dek, &plan)
        .map_err(bad_state)?;
    let account = account::Entity::find_by_id(plan.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("分期计划关联的账户不存在"))?;
    let detail = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let items = installment_item::Entity::find()
        .filter(installment_item::Column::PlanId.eq(id))
        .order_by_asc(installment_item::Column::Sequence)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let account_models = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let repayment_account_names =
        super::bills::account_display_names(&dek, &account_models, &account_details);
    let repayment_account_models = account_models.into_iter().filter(|candidate| {
        crate::investment_funds::is_money_account(candidate)
            && !matches!(candidate.kind.as_str(), "credit_card" | "credit_service")
            && candidate.currency == account.currency
    });
    let repayment_accounts = repayment_account_models
        .into_iter()
        .map(|candidate| RepaymentAccountOption {
            id: candidate.id,
            name: repayment_account_names
                .get(&candidate.id)
                .cloned()
                .unwrap_or_default(),
        })
        .collect();
    let today = time_zone.today();
    let effective_items = items.iter().filter(|item| {
        item.paid_at.is_some()
            || (plan.deleted_at.is_none() && !refund.full_refund && !refund.needs_review)
    });
    let (total_interest, total_fee, total_repayment) = sum_items(&dek, effective_items.clone())?;
    let annual_amounts = annual_repayments(&dek, effective_items)?;
    let review_schedule = items
        .iter()
        .filter(|item| item.paid_at.is_none())
        .map(|item| ReviewRow {
            id: item.id,
            sequence: item.sequence,
            due_date: item.due_date.to_string(),
            principal: Decimal::new(crypto::decrypt_cents(&dek, &item.principal), 2).to_string(),
            interest: Decimal::new(crypto::decrypt_cents(&dek, &item.interest), 2).to_string(),
            fee: Decimal::new(crypto::decrypt_cents(&dek, &item.fee), 2).to_string(),
        })
        .collect();
    let current_annual = annual_row(
        today.year(),
        annual_amounts
            .get(&today.year())
            .copied()
            .unwrap_or_default(),
        &account.currency,
    )?;
    let annual_schedule = annual_amounts
        .into_iter()
        .map(|(year, amounts)| annual_row(year, amounts, &account.currency))
        .collect::<HandlerResult<Vec<_>>>()?;
    let schedule = items
        .into_iter()
        .map(|item| ScheduleRow {
            id: item.id,
            sequence: item.sequence,
            due_date: item.due_date.format("%Y-%m-%d").to_string(),
            principal: currency::format(
                crypto::decrypt_cents(&dek, &item.principal),
                &account.currency,
            ),
            interest: currency::format(
                crypto::decrypt_cents(&dek, &item.interest),
                &account.currency,
            ),
            fee: currency::format(crypto::decrypt_cents(&dek, &item.fee), &account.currency),
            total: currency::format(crypto::decrypt_cents(&dek, &item.total), &account.currency),
            paid: item.paid_at.is_some(),
            overdue: plan.deleted_at.is_none()
                && !refund.full_refund
                && !refund.needs_review
                && item.paid_at.is_none()
                && item.due_date < today,
            repayment_account_name: item
                .repayment_account_id
                .and_then(|id| repayment_account_names.get(&id).cloned())
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let total_records = schedule.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let schedule = schedule
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();
    let title = bill
        .map(|bill| bill_title(&dek, bill))
        .unwrap_or_else(|| "最低还款余款分期".into());
    Ok(Html(
        InstallmentDetailTemplate {
            title,
            bill_id: plan.bill_id,
            deleted: plan.deleted_at.is_some(),
            refunded: plan.deleted_at.is_none() && refund.full_refund,
            needs_review: plan.deleted_at.is_none() && refund.needs_review,
            full_refund: refund.full_refund,
            refunded_cents: refund.refunded,
            review_schedule,
            account_name: super::bills::account_display_name(&dek, &account, detail.as_ref()),
            currency: account.currency.clone(),
            principal: currency::format(plan_principal(&dek, &plan, bill)?, &account.currency),
            term: if total_records == plan.term_months as usize {
                term_label(plan.term_months)
            } else {
                format!(
                    "原{} · 核对后共 {} 期",
                    term_label(plan.term_months),
                    total_records
                )
            },
            method: method_label(&plan.method).into(),
            annual_rate: format!(
                "{}%",
                Decimal::new(crypto::decrypt_cents(&dek, &plan.annual_rate_bps), 2)
            ),
            total_interest: currency::format(total_interest, &account.currency),
            total_fee: currency::format(total_fee, &account.currency),
            total_repayment: currency::format(total_repayment, &account.currency),
            first_due_date: plan.first_due_date.format("%Y-%m-%d").to_string(),
            schedule,
            current_annual,
            annual_schedule,
            repayment_accounts,
            plan_id: id,
            per_page: pagination.per_page,
            total_records,
            pagination: super::pagination_view(
                &pagination,
                total_records,
                &format!("/installments/{id}"),
                "期",
                std::iter::empty::<(String, String)>(),
            ),
        }
        .render()
        .map_err(err500)?,
    ))
}

#[derive(Deserialize)]
pub struct DeleteFormData {
    refunded_cents: i64,
    #[serde(default)]
    treat_as_return: bool,
    /// 全额退款的分期允许把计划、期次、自动流水、原消费与退款一并物理删除。
    #[serde(default)]
    hard: bool,
}

#[derive(Template)]
#[template(path = "installment_delete.html")]
struct DeleteTemplate {
    plan_id: i64,
    title: String,
    account_name: String,
    currency: String,
    refunded_cents: i64,
    refunded_amount: String,
    remaining_amount: String,
    has_refund: bool,
    full_refund: bool,
    deleted: bool,
    refund_authorized: bool,
}

pub async fn delete_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(crate::privacy::PrivacyAccess(unlocked)): Extension<crate::privacy::PrivacyAccess>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let plan = installment_plan::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期计划不存在".into()))?;
    let bill_id = plan.bill_id.ok_or_else(|| {
        bad_request(
            "最低还款余款分期不是单笔消费，不能按退货删除；未还任何期次时请在详情撤销余款分期转换",
        )
    })?;
    let original = bill::Entity::find_by_id(bill_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原消费不存在"))?;
    let (_, refunded, remaining) =
        super::refunds::refund_totals(&state.db, &dek, &original).await?;
    let account = account::Entity::find_by_id(original.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原账户不存在"))?;
    let detail = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    Ok(Html(
        DeleteTemplate {
            plan_id: id,
            title: bill_title(&dek, &original),
            account_name: super::bills::account_display_name(&dek, &account, detail.as_ref()),
            currency: account.currency.clone(),
            refunded_cents: refunded,
            refunded_amount: currency::format(refunded, &account.currency),
            remaining_amount: currency::format(remaining, &account.currency),
            has_refund: refunded > 0,
            full_refund: refunded > 0 && remaining == 0,
            deleted: plan.deleted_at.is_some(),
            refund_authorized: !original.is_private || unlocked,
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(crate::privacy::PrivacyAccess(unlocked)): Extension<crate::privacy::PrivacyAccess>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteFormData>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let tx = state.db.begin().await.map_err(err500)?;
    let plan = installment_plan::Entity::find_by_id(id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期计划不存在".into()))?;
    let bill_id = plan
        .bill_id
        .ok_or_else(|| bad_request("最低还款余款分期不能按退货删除，请使用撤销余款分期转换"))?;
    // 软删除重复提交保持幂等重放，不再校验退款基线；硬删除仍需继续核对。
    if plan.deleted_at.is_some() && !form.hard {
        return Ok(Redirect::to(&format!("/installments/{id}")));
    }
    let original = bill::Entity::find_by_id(bill_id)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原消费不存在"))?;
    let (_, refunded, remaining) = super::refunds::refund_totals(&tx, &dek, &original).await?;
    if refunded != form.refunded_cents {
        return Err(bad_request("退款已变更，请刷新后重新确认"));
    }
    let full_refund = refunded > 0 && remaining == 0;
    if form.hard {
        // 已全额退款（含此前仅软删的历史计划）才允许物理删除整条交易链。
        if !full_refund {
            return Err(bad_request(
                "只有已全额退款的分期可以硬删除，其余只能软删除",
            ));
        }
        hard_delete_returned_plan(&tx, &dek, &plan, &original, unlocked).await?;
        tx.commit().await.map_err(err500)?;
        return Ok(Redirect::to("/installments"));
    }
    if refunded == 0 {
        if !form.treat_as_return {
            return Err(bad_request(
                "尚无退款；必须明确确认已实际收到全额退款，才能补记退款并删除分期",
            ));
        }
        super::refunds::record_refund_on(
            &tx,
            &dek,
            &original,
            unlocked,
            remaining,
            chrono::Utc::now().naive_utc(),
            "确认实际全额退货退款并删除分期",
        )
        .await?;
    }
    let mut active = plan.into_active_model();
    active.deleted_at = Set(Some(chrono::Utc::now()));
    active.update(&tx).await.map_err(err500)?;
    tx.commit().await.map_err(err500)?;
    Ok(Redirect::to(&format!("/installments/{id}")))
}
/// 全额退款视为整笔交易取消：在同一金融事务中物理删除分期计划、全部期次、
/// 期次自动本金转账与费用账单、原消费及其全部退款流水；按账户合并净影响
/// 校验余额，删除后账面回到交易发生前，不产生任何调整流水。
async fn hard_delete_returned_plan(
    tx: &sea_orm::DatabaseTransaction,
    dek: &crypto::Dek,
    plan: &installment_plan::Model,
    original: &bill::Model,
    unlocked: bool,
) -> HandlerResult<()> {
    super::bills::require_bill_access(original, unlocked)?;
    if plan.repayment_transfer_id.is_some() {
        return Err(bad_request("最低还款转换关联的分期不能按退货硬删除"));
    }
    let items = installment_item::Entity::find()
        .filter(installment_item::Column::PlanId.eq(plan.id))
        .all(tx)
        .await
        .map_err(err500)?;
    let refunds = bill::Entity::find()
        .filter(bill::Column::RefundOfBillId.eq(original.id))
        .all(tx)
        .await
        .map_err(err500)?;
    let mut balance_changes: HashMap<i64, i64> = HashMap::new();
    let mut add_change = |account_id: i64, delta: i64| -> HandlerResult<()> {
        let entry = balance_changes.entry(account_id).or_default();
        *entry = checked_add(*entry, delta)?;
        Ok(())
    };
    for item in &items {
        if let Some(transfer_id) = item.principal_transfer_id {
            let transfer = transfer::Entity::find_by_id(transfer_id)
                .one(tx)
                .await
                .map_err(err500)?
                .ok_or_else(|| bad_request("期次本金转账不存在"))?;
            add_change(
                transfer.from_account_id,
                crypto::decrypt_cents(dek, &transfer.amount),
            )?;
            let to_change = super::transfer_to_cents(dek, &transfer)
                .checked_neg()
                .ok_or_else(|| bad_request("还款金额超出范围"))?;
            add_change(transfer.to_account_id, to_change)?;
        }
        if let Some(charge_id) = item.charge_bill_id {
            if bill::Entity::find()
                .filter(bill::Column::RefundOfBillId.eq(charge_id))
                .one(tx)
                .await
                .map_err(err500)?
                .is_some()
            {
                return Err(bad_request(
                    "分期费用账单已有退款，请先删除费用退款记录再硬删除",
                ));
            }
            let charge = bill::Entity::find_by_id(charge_id)
                .one(tx)
                .await
                .map_err(err500)?
                .ok_or_else(|| bad_request("期次费用账单不存在"))?;
            super::bills::require_bill_access(&charge, unlocked)?;
            add_change(
                charge.account_id,
                crypto::decrypt_cents(dek, &charge.amount),
            )?;
        }
    }
    for refund in &refunds {
        let change = crypto::decrypt_cents(dek, &refund.amount)
            .checked_neg()
            .ok_or_else(|| bad_request("退款金额超出范围"))?;
        add_change(refund.account_id, change)?;
    }
    add_change(
        original.account_id,
        crypto::decrypt_cents(dek, &original.amount),
    )?;
    for (account_id, delta) in balance_changes {
        super::accounts::ensure_balance_delta_on(tx, dek, account_id, delta).await?;
    }
    for item in &items {
        if let Some(transfer_id) = item.principal_transfer_id {
            transfer::Entity::delete_by_id(transfer_id)
                .exec(tx)
                .await
                .map_err(err500)?;
        }
        if let Some(charge_id) = item.charge_bill_id {
            bill::Entity::delete_by_id(charge_id)
                .exec(tx)
                .await
                .map_err(err500)?;
        }
    }
    installment_item::Entity::delete_many()
        .filter(installment_item::Column::PlanId.eq(plan.id))
        .exec(tx)
        .await
        .map_err(err500)?;
    installment_plan::Entity::delete_by_id(plan.id)
        .exec(tx)
        .await
        .map_err(err500)?;
    for refund in &refunds {
        bill::Entity::delete_by_id(refund.id)
            .exec(tx)
            .await
            .map_err(err500)?;
    }
    bill::Entity::delete_by_id(original.id)
        .exec(tx)
        .await
        .map_err(err500)?;
    Ok(())
}

pub async fn set_paid(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<PaidFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let transaction = state.db.begin().await.map_err(err500)?;
    let item = installment_item::Entity::find_by_id(id)
        .one(&transaction)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分期期次不存在".into()))?;
    let plan_id = item.plan_id;
    if form.paid {
        let repayment_account_id = form
            .repayment_account_id
            .ok_or_else(|| bad_request("请选择还款渠道"))?;
        let plan = installment_plan::Entity::find_by_id(plan_id)
            .one(&transaction)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("分期计划不存在"))?;
        pay_items(
            &transaction,
            &dek,
            &plan,
            &[item],
            repayment_account_id,
            chrono::Utc::now().naive_utc(),
        )
        .await?;
    } else {
        if item.paid_at.is_none() {
            return Err(bad_request("该期尚未还款"));
        }
        let mut balance_changes: HashMap<i64, i64> = HashMap::new();
        if let Some(transfer_id) = item.principal_transfer_id {
            if let Some(principal_transfer) = transfer::Entity::find_by_id(transfer_id)
                .one(&transaction)
                .await
                .map_err(err500)?
            {
                balance_changes.insert(
                    principal_transfer.from_account_id,
                    crypto::decrypt_cents(&dek, &principal_transfer.amount),
                );
                let to_change = super::transfer_to_cents(&dek, &principal_transfer)
                    .checked_neg()
                    .ok_or_else(|| bad_request("还款金额超出范围"))?;
                let change = balance_changes
                    .entry(principal_transfer.to_account_id)
                    .or_default();
                *change = checked_add(*change, to_change)?;
            }
        }
        if let Some(bill_id) = item.charge_bill_id {
            if bill::Entity::find()
                .filter(bill::Column::RefundOfBillId.eq(bill_id))
                .one(&transaction)
                .await
                .map_err(err500)?
                .is_some()
            {
                return Err(bad_request("已有退款的费用账单不能删除，请先删除退款记录"));
            }
            if let Some(charge_bill) = bill::Entity::find_by_id(bill_id)
                .one(&transaction)
                .await
                .map_err(err500)?
            {
                let change = balance_changes.entry(charge_bill.account_id).or_default();
                *change = checked_add(*change, crypto::decrypt_cents(&dek, &charge_bill.amount))?;
            }
        }
        for (account_id, change) in balance_changes {
            super::accounts::ensure_balance_delta_on(&transaction, &dek, account_id, change)
                .await?;
        }
        if let Some(transfer_id) = item.principal_transfer_id {
            transfer::Entity::delete_by_id(transfer_id)
                .exec(&transaction)
                .await
                .map_err(err500)?;
        }
        if let Some(bill_id) = item.charge_bill_id {
            bill::Entity::delete_by_id(bill_id)
                .exec(&transaction)
                .await
                .map_err(err500)?;
        }
        let mut active = item.into_active_model();
        active.paid_at = Set(None);
        active.repayment_account_id = Set(None);
        active.principal_transfer_id = Set(None);
        active.charge_bill_id = Set(None);
        active.update(&transaction).await.map_err(err500)?;
    }
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to(&format!("/installments/{plan_id}")))
}

#[cfg(test)]
mod annual_tests {
    use super::*;

    #[test]
    fn legacy_return_candidates_include_partial_full_and_reviewed_plans() {
        let mut plan = installment_plan::Model {
            id: 1,
            bill_id: Some(1),
            account_id: 1,
            principal: String::new(),
            repayment_transfer_id: None,
            converted_at: None,
            refund_reviewed_amount: String::new(),
            term_months: 3,
            method: "flat".into(),
            annual_rate_bps: String::new(),
            fee: String::new(),
            first_due_date: NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            deleted_at: None,
            created_at: chrono::Utc::now(),
        };
        for (refunded, full_refund, needs_review, expected) in [
            (0, false, false, false),
            (6000, false, true, true),
            (6000, false, false, true),
            (30000, true, false, true),
        ] {
            let refund = crate::installment_state::RefundState {
                refunded,
                full_refund,
                needs_review,
            };
            // Candidate detection deliberately does not depend on whether any
            // unpaid installments remain.
            assert_eq!(is_returned_plan(&plan, &refund), expected);
            plan.deleted_at = Some(chrono::Utc::now());
            assert!(!is_returned_plan(&plan, &refund));
            plan.deleted_at = None;
            plan.bill_id = None;
            assert!(!is_returned_plan(&plan, &refund));
            plan.bill_id = Some(1);
        }
    }

    fn item(
        dek: &crypto::Dek,
        due_date: &str,
        principal: i64,
        interest: i64,
        fee: i64,
        paid_year: Option<i32>,
    ) -> installment_item::Model {
        installment_item::Model {
            id: 1,
            plan_id: 1,
            sequence: 1,
            due_date: NaiveDate::parse_from_str(due_date, "%Y-%m-%d").unwrap(),
            principal: crypto::encrypt_cents(dek, principal),
            interest: crypto::encrypt_cents(dek, interest),
            fee: crypto::encrypt_cents(dek, fee),
            total: crypto::encrypt_cents(dek, principal + interest + fee),
            paid_at: paid_year.map(|year| {
                NaiveDate::from_ymd_opt(year, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
            }),
            repayment_account_id: None,
            principal_transfer_id: None,
            charge_bill_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn annual_repayments_follow_due_year_include_fees_and_keep_unpaid_overdue() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let mut items = vec![
            item(&dek, "2025-12-31", 10_000, 100, 20, None),
            item(&dek, "2026-01-01", 20_000, 200, 30, None),
            item(&dek, "2026-12-31", 30_000, 300, 40, Some(2025)),
            item(&dek, "2027-01-01", 40_000, 400, 50, Some(2026)),
        ];
        let years = annual_repayments(&dek, &items).unwrap();
        assert_eq!(years.len(), 3);
        assert_eq!(
            years[&2025],
            AnnualAmounts {
                total: 10_120,
                unpaid: 10_120
            }
        );
        assert_eq!(
            years[&2026],
            AnnualAmounts {
                total: 50_570,
                unpaid: 20_230
            }
        );
        assert_eq!(years[&2026].paid().unwrap(), 30_340);
        assert_eq!(
            years[&2027],
            AnnualAmounts {
                total: 40_450,
                unpaid: 0
            }
        );
        items[1].paid_at = Some(chrono::Utc::now());
        let paid_years = annual_repayments(&dek, &items).unwrap();
        assert_eq!(paid_years[&2026].total, years[&2026].total);
        assert_eq!(paid_years[&2026].unpaid, 0);
        assert_eq!(paid_years[&2026].paid().unwrap(), 50_570);
        assert_eq!(paid_years[&2025], years[&2025]);
        assert_eq!(paid_years[&2027], years[&2027]);
    }

    #[test]
    fn annual_repayments_reject_overflow_in_year_and_combined_plan_totals() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let items = [
            item(&dek, "2026-01-01", i64::MAX, 0, 0, None),
            item(&dek, "2026-02-01", 1, 0, 0, None),
        ];
        assert!(annual_repayments(&dek, &items).is_err());
        let mut combined = AnnualAmounts {
            total: i64::MAX,
            unpaid: i64::MAX,
        };
        assert!(combined.add(1, 0).is_err());
        assert!(combined.add(0, 1).is_err());
        assert_eq!(
            combined,
            AnnualAmounts {
                total: i64::MAX,
                unpaid: i64::MAX
            }
        );
    }
}
