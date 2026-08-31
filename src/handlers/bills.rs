use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{
        account, account_detail, balance_adjustment, bill, category, debt_person, debt_record,
        installment_item, installment_plan, investment_execution, transfer,
    },
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
    pub(crate) detail_url: String,
    pub(crate) delete_action: String,
    pub(crate) delete_confirm: String,
    pub(crate) sort_key: NaiveDateTime,
    pub(crate) bill_kind: String,
    pub(crate) amount_cents: i64,
    pub(crate) is_expense: bool,
}

#[derive(Clone)]
struct AccountOption {
    id: i64,
    name: String,
    kind: String,
    currency: String,
}

struct CategoryOption {
    kind: String,
    name: String,
}

struct PersonOption {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "bills.html")]
struct BillsTemplate {
    page_heading: String,
    advanced_search: bool,
    search_action: String,
    records: Vec<LedgerRow>,
    total_income: String,
    total_expense: String,
    net: String,
    search_mode: String,
    start_date: String,
    end_date: String,
    flow_kind: String,
    category: String,
    min_income: String,
    max_income: String,
    min_expense: String,
    max_expense: String,
    keyword: String,
    has_filters: bool,
    search_categories: Vec<CategoryOption>,
    default_currency: String,
    per_page: usize,
    total_records: usize,
    pagination: super::PaginationView,
}

#[derive(Template)]
#[template(path = "quick_entry_page.html")]
struct QuickEntryPageTemplate {
    accounts: Vec<AccountOption>,
    transfer_sources: Vec<AccountOption>,
    people: Vec<PersonOption>,
    categories: Vec<CategoryOption>,
    happened_at: String,
    quick_entry_heading: String,
    quick_redirect_to: String,
    first_due_date: String,
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
    use_installment: bool,
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
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteFormData {
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct BillsQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
    #[serde(default)]
    flow_kind: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    min_income: String,
    #[serde(default)]
    max_income: String,
    #[serde(default)]
    min_expense: String,
    #[serde(default)]
    max_expense: String,
    #[serde(default)]
    keyword: String,
}

struct LedgerFilter {
    mode_or: bool,
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
    flow_kind: String,
    category: String,
    min_income: Option<i64>,
    max_income: Option<i64>,
    min_expense: Option<i64>,
    max_expense: Option<i64>,
    keyword: String,
}

fn ledger_redirect(redirect_to: Option<&str>) -> &'static str {
    if redirect_to == Some("/dashboard") {
        "/dashboard"
    } else {
        "/bills"
    }
}

fn parse_search_date(value: &str, label: &str) -> HandlerResult<Option<chrono::NaiveDate>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    chrono::NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map(Some)
        .map_err(|_| bad_request(&format!("{label}格式不正确")))
}

fn parse_search_amount(value: &str, label: &str) -> HandlerResult<Option<i64>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request(&format!("{label}格式不正确")))?
        .round_dp(2);
    if decimal < Decimal::ZERO {
        return Err(bad_request(&format!("{label}不能小于 0")));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .map(Some)
        .ok_or_else(|| bad_request(&format!("{label}超出范围")))
}

impl LedgerFilter {
    fn from_query(query: &BillsQuery) -> HandlerResult<Self> {
        let start_date = parse_search_date(&query.start_date, "开始日期")?;
        let end_date = parse_search_date(&query.end_date, "结束日期")?;
        if start_date
            .zip(end_date)
            .is_some_and(|(start, end)| start > end)
        {
            return Err(bad_request("开始日期不能晚于结束日期"));
        }
        let flow_kind = match query.flow_kind.as_str() {
            "income" | "expense" => query.flow_kind.clone(),
            _ => String::new(),
        };
        let min_income = parse_search_amount(&query.min_income, "最低收入")?;
        let max_income = parse_search_amount(&query.max_income, "最高收入")?;
        if min_income
            .zip(max_income)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(bad_request("最低收入不能大于最高收入"));
        }
        let min_expense = parse_search_amount(&query.min_expense, "最低支出")?;
        let max_expense = parse_search_amount(&query.max_expense, "最高支出")?;
        if min_expense
            .zip(max_expense)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(bad_request("最低支出不能大于最高支出"));
        }
        Ok(Self {
            mode_or: query.mode == "or",
            start_date,
            end_date,
            flow_kind,
            category: query.category.trim().to_string(),
            min_income,
            max_income,
            min_expense,
            max_expense,
            keyword: query.keyword.trim().to_lowercase(),
        })
    }

    fn is_active(&self) -> bool {
        self.start_date.is_some()
            || self.end_date.is_some()
            || !self.flow_kind.is_empty()
            || !self.category.is_empty()
            || self.min_income.is_some()
            || self.max_income.is_some()
            || self.min_expense.is_some()
            || self.max_expense.is_some()
            || !self.keyword.is_empty()
    }

    fn matches(&self, row: &LedgerRow) -> bool {
        let mut conditions = Vec::with_capacity(3);
        if self.start_date.is_some() || self.end_date.is_some() {
            let date = row.sort_key.date();
            conditions.push(
                self.start_date.is_none_or(|start| date >= start)
                    && self.end_date.is_none_or(|end| date <= end),
            );
        }
        if !self.flow_kind.is_empty() {
            conditions.push(row.bill_kind == self.flow_kind);
        }
        if !self.category.is_empty() {
            conditions.push(!row.bill_kind.is_empty() && row.subject == self.category);
        }
        let income_range = self.min_income.is_some() || self.max_income.is_some();
        let expense_range = self.min_expense.is_some() || self.max_expense.is_some();
        if income_range || expense_range {
            let income_matches = income_range
                && row.bill_kind == "income"
                && self
                    .min_income
                    .is_none_or(|minimum| row.amount_cents >= minimum)
                && self
                    .max_income
                    .is_none_or(|maximum| row.amount_cents <= maximum);
            let expense_matches = expense_range
                && row.is_expense
                && self
                    .min_expense
                    .is_none_or(|minimum| row.amount_cents >= minimum)
                && self
                    .max_expense
                    .is_none_or(|maximum| row.amount_cents <= maximum);
            conditions.push(income_matches || expense_matches);
        }
        if !self.keyword.is_empty() {
            let haystack = format!(
                "{} {} {} {}",
                row.record_type, row.account_name, row.subject, row.note
            )
            .to_lowercase();
            conditions.push(haystack.contains(&self.keyword));
        }
        if conditions.is_empty() {
            true
        } else if self.mode_or {
            conditions.into_iter().any(|matches| matches)
        } else {
            conditions.into_iter().all(|matches| matches)
        }
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
    installment: Option<super::installments::NewPlan>,
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
        installment: if form.use_installment {
            Some(super::installments::parse_input(
                &form.installment_term,
                &form.installment_method,
                &form.installment_annual_rate,
                &form.installment_fee,
                &form.installment_first_due_date,
            )?)
        } else {
            None
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
            kind: a.kind,
            currency: a.currency,
        })
        .collect())
}

async fn people_options(state: &AppState, dek: &crypto::Dek) -> HandlerResult<Vec<PersonOption>> {
    Ok(debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|person| PersonOption {
            id: person.id,
            name: crypto::decrypt_string(dek, &person.name),
        })
        .collect())
}

pub(crate) fn account_display_name(
    dek: &crypto::Dek,
    account: &account::Model,
    detail: Option<&account_detail::Model>,
) -> String {
    let name = crypto::decrypt_string(dek, &account.name);
    let identity = detail.and_then(|detail| {
        let card_number = crypto::decrypt_string(dek, &detail.card_number);
        if !card_number.is_empty() {
            return Some(format!("卡号 {}", super::mask_card_number(&card_number)));
        }
        let username = crypto::decrypt_string(dek, &detail.account_username);
        (!username.is_empty())
            .then(|| format!("用户名 {}", super::mask_account_username(&username)))
    });
    match identity {
        Some(identity) => format!("{name} · {identity} · {}", account.currency),
        None => format!("{name} · {}", account.currency),
    }
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
    let default_currency = currency::default_currency(state).await.map_err(err500)?;
    let today = chrono::Local::now().date_naive();
    let currencies = accounts
        .iter()
        .map(|account| account.currency.clone())
        .collect::<Vec<_>>();
    let rates = currency::RateTable::load(state, currencies, &default_currency, today)
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
        .iter()
        .map(|account| {
            (
                account.id,
                account_display_name(dek, &account, details.get(&account.id)),
            )
        })
        .collect();
    let account_currencies: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();
    let person_names: HashMap<i64, String> = debt_person::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|person| (person.id, crypto::decrypt_string(dek, &person.name)))
        .collect();
    let installment_plan_ids: HashMap<i64, i64> = installment_plan::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|plan| (plan.bill_id, plan.id))
        .collect();

    let mut rows = Vec::new();
    for bill in bill::Entity::find().all(&state.db).await.map_err(err500)? {
        let incoming = bill.kind == "income";
        let amount = crypto::decrypt_cents(dek, &bill.amount);
        let bill_currency = account_currencies
            .get(&bill.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let converted = rates.convert(amount, bill_currency).map_err(err500)?;
        rows.push(LedgerRow {
            happened_at: bill.happened_at.format(TIME_FMT).to_string(),
            record_type: if incoming {
                "收入".into()
            } else if installment_plan_ids.contains_key(&bill.id) {
                "支出 · 分期".into()
            } else {
                "支出".into()
            },
            account_name: account_names
                .get(&bill.account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            subject: crypto::decrypt_string(dek, &bill.category),
            note: crypto::decrypt_string(dek, &bill.note),
            amount: format!(
                "{}{}",
                if incoming { "+" } else { "-" },
                currency::format(amount, bill_currency)
            ),
            money_class: if incoming {
                "text-green-600"
            } else {
                "text-red-600"
            }
            .into(),
            edit_url: if installment_plan_ids.contains_key(&bill.id) {
                String::new()
            } else {
                format!("/bills/{}/edit", bill.id)
            },
            detail_url: installment_plan_ids
                .get(&bill.id)
                .map(|plan_id| format!("/installments/{plan_id}"))
                .unwrap_or_default(),
            delete_action: format!("/bills/{}/delete", bill.id),
            delete_confirm: "确认删除这条账单？".into(),
            sort_key: bill.happened_at,
            bill_kind: bill.kind,
            amount_cents: converted,
            is_expense: !incoming,
        });
    }

    for transfer in transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let amount = crypto::decrypt_cents(dek, &transfer.amount);
        let to_amount = super::transfer_to_cents(dek, &transfer);
        let from_currency = account_currencies
            .get(&transfer.from_account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let to_currency = account_currencies
            .get(&transfer.to_account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        rows.push(LedgerRow {
            happened_at: transfer.happened_at.format(TIME_FMT).to_string(),
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
            amount: if from_currency == to_currency {
                currency::format(amount, from_currency)
            } else {
                format!(
                    "{} → {}",
                    currency::format(amount, from_currency),
                    currency::format(to_amount, to_currency)
                )
            },
            money_class: "text-blue-600".into(),
            edit_url: String::new(),
            detail_url: String::new(),
            delete_action: format!("/transfers/{}/delete", transfer.id),
            delete_confirm: "确认删除这条转账？".into(),
            sort_key: transfer.happened_at,
            bill_kind: String::new(),
            amount_cents: rates.convert(amount, from_currency).map_err(err500)?,
            is_expense: false,
        });
    }

    for record in debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let incoming = matches!(record.kind.as_str(), "borrow" | "repayment_received");
        let amount = crypto::decrypt_cents(dek, &record.amount);
        let record_currency = account_currencies
            .get(&record.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let converted = rates.convert(amount, record_currency).map_err(err500)?;
        rows.push(LedgerRow {
            happened_at: record.happened_at.format(TIME_FMT).to_string(),
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
                currency::format(amount, record_currency)
            ),
            money_class: if incoming {
                "text-green-600"
            } else {
                "text-red-600"
            }
            .into(),
            edit_url: String::new(),
            detail_url: String::new(),
            delete_action: format!("/debts/{}/delete", record.id),
            delete_confirm: "确认删除这条借还记录？".into(),
            sort_key: record.happened_at,
            bill_kind: String::new(),
            amount_cents: converted,
            is_expense: false,
        });
    }

    for adjustment in balance_adjustment::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let adjustment_currency = account_currencies
            .get(&adjustment.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let from_balance = crypto::decrypt_cents(dek, &adjustment.from_balance);
        let to_balance = crypto::decrypt_cents(dek, &adjustment.to_balance);
        rows.push(LedgerRow {
            happened_at: adjustment.happened_at.format(TIME_FMT).to_string(),
            record_type: "余额调整".into(),
            account_name: account_names
                .get(&adjustment.account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into()),
            subject: "强制设置余额".into(),
            note: format!(
                "{} → {}",
                currency::format(from_balance, adjustment_currency),
                currency::format(to_balance, adjustment_currency)
            ),
            amount: currency::format(to_balance, adjustment_currency),
            money_class: "text-amber-700".into(),
            edit_url: String::new(),
            detail_url: String::new(),
            delete_action: String::new(),
            delete_confirm: String::new(),
            sort_key: adjustment.happened_at,
            bill_kind: String::new(),
            amount_cents: 0,
            is_expense: false,
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
    Query(query): Query<BillsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, query, false).await
}

pub async fn advanced_search(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<BillsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, query, true).await
}

async fn render_list(
    state: &AppState,
    dek: &crypto::Dek,
    mut query: BillsQuery,
    advanced_search: bool,
) -> HandlerResult<Html<String>> {
    if !advanced_search {
        query.mode = "and".into();
        query.flow_kind.clear();
        query.category.clear();
        query.min_income.clear();
        query.max_income.clear();
        query.min_expense.clear();
        query.max_expense.clear();
    }
    let filter = LedgerFilter::from_query(&query)?;
    let has_filters = filter.is_active();
    let default_currency = currency::default_currency(state).await.map_err(err500)?;
    let all_records = ledger_rows(state, dek)
        .await?
        .into_iter()
        .filter(|row| filter.matches(row))
        .collect::<Vec<_>>();
    let mut total_income: i64 = 0;
    let mut total_expense: i64 = 0;
    for row in &all_records {
        if row.bill_kind == "income" {
            total_income = total_income
                .checked_add(row.amount_cents)
                .ok_or_else(|| err500("汇总金额超出范围"))?;
        } else if row.bill_kind == "expense" {
            total_expense = total_expense
                .checked_add(row.amount_cents)
                .ok_or_else(|| err500("汇总金额超出范围"))?;
        }
    }

    let net = total_income
        .checked_sub(total_expense)
        .ok_or_else(|| err500("汇总金额超出范围"))?;
    let total_records = all_records.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let records = all_records
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect::<Vec<_>>();
    let search_categories = if advanced_search {
        category_options(state, dek).await?
    } else {
        Vec::new()
    };
    let html = BillsTemplate {
        page_heading: if advanced_search {
            "高级搜索".into()
        } else {
            "账单".into()
        },
        advanced_search,
        search_action: if advanced_search {
            "/bills/search".into()
        } else {
            "/bills".into()
        },
        records,
        total_income: currency::format(total_income, &default_currency),
        total_expense: currency::format(total_expense, &default_currency),
        net: currency::format(net, &default_currency),
        search_mode: if query.mode == "or" { "or" } else { "and" }.into(),
        start_date: query.start_date.clone(),
        end_date: query.end_date.clone(),
        flow_kind: query.flow_kind.clone(),
        category: query.category.clone(),
        min_income: query.min_income.clone(),
        max_income: query.max_income.clone(),
        min_expense: query.min_expense.clone(),
        max_expense: query.max_expense.clone(),
        keyword: query.keyword.clone(),
        has_filters,
        search_categories,
        default_currency,
        per_page: pagination.per_page,
        total_records,
        pagination: super::pagination_view(
            &pagination,
            total_records,
            if advanced_search {
                "/bills/search"
            } else {
                "/bills"
            },
            "条流水",
            [
                ("mode", query.mode.clone()),
                ("start_date", query.start_date.clone()),
                ("end_date", query.end_date.clone()),
                ("flow_kind", query.flow_kind.clone()),
                ("category", query.category.clone()),
                ("min_income", query.min_income.clone()),
                ("max_income", query.max_income.clone()),
                ("min_expense", query.min_expense.clone()),
                ("max_expense", query.max_expense.clone()),
                ("keyword", query.keyword.clone()),
            ],
        ),
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
    let transfer_sources = accounts
        .iter()
        .filter(|account| !matches!(account.kind.as_str(), "credit_card" | "credit_service"))
        .cloned()
        .collect();
    let html = QuickEntryPageTemplate {
        accounts,
        transfer_sources,
        people: people_options(&state, &dek).await?,
        categories: category_options(&state, &dek).await?,
        happened_at: chrono::Utc::now().naive_utc().format(TIME_FMT).to_string(),
        quick_entry_heading: "记一笔".into(),
        quick_redirect_to: "/bills".into(),
        first_due_date: (chrono::Local::now().date_naive() + chrono::Months::new(1))
            .format("%Y-%m-%d")
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
    let account = account::Entity::find_by_id(parsed.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("账户不存在"))?;
    if parsed.installment.is_some()
        && (parsed.kind != "expense"
            || !matches!(account.kind.as_str(), "credit_card" | "credit_service"))
    {
        return Err(bad_request("只有信用卡或信贷服务的支出可以设置分期"));
    }
    let is_food = category_is_food(&state, &dek, &parsed.kind, &parsed.category).await?;
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        parsed.account_id,
        signed_amount(&parsed.kind, parsed.amount)?,
    )
    .await?;
    let transaction = state.db.begin().await.map_err(err500)?;
    let created_bill = bill::ActiveModel {
        account_id: Set(parsed.account_id),
        kind: Set(parsed.kind.clone()),
        amount: Set(crypto::encrypt_cents(&dek, parsed.amount)),
        category: Set(crypto::encrypt(&dek, parsed.category.as_bytes())),
        is_food: Set(is_food),
        note: Set(crypto::encrypt(&dek, parsed.note.as_bytes())),
        happened_at: Set(parsed.happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    let plan_id = if let Some(installment) = parsed.installment {
        Some(
            super::installments::create_plan(
                &transaction,
                &dek,
                created_bill.id,
                parsed.account_id,
                parsed.amount,
                installment,
            )
            .await?,
        )
    } else {
        None
    };
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to(
        &plan_id
            .map(|id| format!("/installments/{id}"))
            .unwrap_or(redirect_to),
    ))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    if investment_execution::Entity::find()
        .filter(investment_execution::Column::FeeBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "定投手续费流水不能单独编辑；删除定投计划不会影响已有手续费流水",
        ));
    }
    if installment_item::Entity::find()
        .filter(installment_item::Column::ChargeBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "分期还款费用流水不能单独编辑，请在分期详情中撤销后重新还款",
        ));
    }
    if installment_plan::Entity::find()
        .filter(installment_plan::Column::BillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "分期账单不能直接编辑；如需重建计划，请删除账单后重新记录",
        ));
    }
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
    if investment_execution::Entity::find()
        .filter(investment_execution::Column::FeeBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "定投手续费流水不能单独编辑；删除定投计划不会影响已有手续费流水",
        ));
    }
    if installment_item::Entity::find()
        .filter(installment_item::Column::ChargeBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "分期还款费用流水不能单独编辑，请在分期详情中撤销后重新还款",
        ));
    }
    if installment_plan::Entity::find()
        .filter(installment_plan::Column::BillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "分期账单不能直接编辑；如需重建计划，请删除账单后重新记录",
        ));
    }
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
    if investment_execution::Entity::find()
        .filter(investment_execution::Column::FeeBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "定投手续费流水不能单独删除；删除定投计划不会影响已有手续费流水",
        ));
    }
    if installment_item::Entity::find()
        .filter(installment_item::Column::ChargeBillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request(
            "分期还款费用流水不能单独删除，请在分期详情中撤销对应还款",
        ));
    }
    let b = bill::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    if let Some(plan) = installment_plan::Entity::find()
        .filter(installment_plan::Column::BillId.eq(id))
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        if installment_item::Entity::find()
            .filter(installment_item::Column::PlanId.eq(plan.id))
            .filter(installment_item::Column::PaidAt.is_not_null())
            .one(&state.db)
            .await
            .map_err(err500)?
            .is_some()
        {
            return Err(bad_request(
                "该分期已有实际还款，请先在分期详情逐期撤销还款，再删除原账单",
            ));
        }
    }
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
