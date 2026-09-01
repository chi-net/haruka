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
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{account, debt_person, debt_record},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct DebtPersonRow {
    id: i64,
    name: String,
    note: String,
    receivable: String,
    payable: String,
    receivable_cents: i64,
    payable_cents: i64,
}

#[derive(Template)]
#[template(path = "debt_people.html")]
struct DebtPeopleTemplate {
    page_heading: String,
    advanced_search: bool,
    search_action: String,
    people: Vec<DebtPersonRow>,
    default_currency: String,
    mode: String,
    keyword: String,
    relationship: String,
    min_receivable: String,
    max_receivable: String,
    min_payable: String,
    max_payable: String,
    has_filters: bool,
    per_page: usize,
    pagination: super::PaginationView,
}

#[derive(Template)]
#[template(path = "debt_person_form.html")]
struct DebtPersonFormTemplate {
    heading: String,
    action: String,
    name: String,
    note: String,
}

#[derive(Deserialize)]
pub struct DebtRecordFormData {
    person_id: i64,
    account_id: i64,
    kind: String,
    amount: String,
    note: String,
    happened_at: String,
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Serialize)]
pub struct DebtCreateResponse {
    ok: bool,
    message: String,
    redirect: String,
}

#[derive(Deserialize)]
pub struct DeleteRecordFormData {
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
pub struct DebtPersonFormData {
    name: String,
    note: String,
}

#[derive(Default, Deserialize)]
pub struct DebtPeopleQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    relationship: String,
    #[serde(default)]
    min_receivable: String,
    #[serde(default)]
    max_receivable: String,
    #[serde(default)]
    min_payable: String,
    #[serde(default)]
    max_payable: String,
}

fn valid_kind(kind: &str) -> bool {
    matches!(
        kind,
        "lend" | "borrow" | "repayment_received" | "repayment_paid"
    )
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
}

fn account_delta(kind: &str, amount: i64) -> HandlerResult<i64> {
    if matches!(kind, "borrow" | "repayment_received") {
        Ok(amount)
    } else {
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))
    }
}

fn parse_amount(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("金额格式不正确"))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request("金额必须大于 0"));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("金额超出范围"))
}

fn parse_optional_amount(value: &str, label: &str) -> HandlerResult<Option<i64>> {
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

fn apply_outstanding(
    kind: &str,
    amount: i64,
    receivable: &mut i64,
    payable: &mut i64,
) -> HandlerResult<()> {
    match kind {
        "lend" => {
            *receivable = receivable
                .checked_add(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        "repayment_received" => {
            *receivable = receivable
                .checked_sub(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        "borrow" => {
            *payable = payable
                .checked_add(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        "repayment_paid" => {
            *payable = payable
                .checked_sub(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        _ => {}
    }
    Ok(())
}

async fn person_outstanding(
    state: &AppState,
    dek: &crypto::Dek,
    person_id: i64,
) -> HandlerResult<(i64, i64)> {
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_currencies: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();
    let default_currency = currency::default_currency(state).await.map_err(err500)?;
    let currencies = accounts
        .into_iter()
        .map(|account| account.currency)
        .collect::<Vec<_>>();
    let rates = currency::RateTable::load(
        state,
        currencies,
        &default_currency,
        chrono::Local::now().date_naive(),
    )
    .await
    .map_err(err500)?;
    let records = debt_record::Entity::find()
        .filter(debt_record::Column::PersonId.eq(person_id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut receivable = 0;
    let mut payable = 0;
    for record in records {
        let native_amount = crypto::decrypt_cents(dek, &record.amount);
        let record_currency = account_currencies
            .get(&record.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        apply_outstanding(
            &record.kind,
            rates
                .convert(native_amount, record_currency)
                .map_err(err500)?,
            &mut receivable,
            &mut payable,
        )?;
    }
    Ok((receivable, payable))
}

pub async fn create_record(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    headers: HeaderMap,
    Form(form): Form<DebtRecordFormData>,
) -> HandlerResult<Response> {
    let _balance_guard = state.balance_writes.lock().await;
    if !valid_kind(&form.kind) {
        return Err(bad_request("借还类型无效"));
    }
    if debt_person::Entity::find_by_id(form.person_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err(bad_request("借贷对象不存在"));
    }
    let account = account::Entity::find_by_id(form.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("账户不存在"))?;
    let amount = parse_amount(&form.amount)?;
    let (receivable, payable) = person_outstanding(&state, &dek, form.person_id).await?;
    let amount_in_default = currency::convert_cents(
        &state,
        amount,
        &account.currency,
        &currency::default_currency(&state).await.map_err(err500)?,
        chrono::Local::now().date_naive(),
    )
    .await
    .map_err(err500)?;
    if form.kind == "repayment_received" && amount_in_default > receivable {
        return Err(bad_request("还款金额超过对方尚欠金额"));
    }
    if form.kind == "repayment_paid" && amount_in_default > payable {
        return Err(bad_request("还款金额超过尚欠对方金额"));
    }
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        form.account_id,
        account_delta(&form.kind, amount)?,
    )
    .await?;
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    debt_record::ActiveModel {
        person_id: Set(form.person_id),
        account_id: Set(form.account_id),
        kind: Set(form.kind),
        amount: Set(crypto::encrypt_cents(&dek, amount)),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        happened_at: Set(happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    let redirect = if form.redirect_to.as_deref() == Some("/bills") {
        "/bills"
    } else {
        "/dashboard"
    };
    if accepts_json(&headers) {
        Ok(Json(DebtCreateResponse {
            ok: true,
            message: "借还记录已保存".into(),
            redirect: redirect.into(),
        })
        .into_response())
    } else {
        Ok(Redirect::to(redirect).into_response())
    }
}

pub async fn delete_record(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteRecordFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let record = debt_record::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还记录不存在".into()))?;
    let delta = account_delta(&record.kind, crypto::decrypt_cents(&dek, &record.amount))?
        .checked_neg()
        .ok_or_else(|| bad_request("金额超出范围"))?;
    super::accounts::ensure_balance_delta(&state, &dek, record.account_id, delta).await?;
    debt_record::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to(
        if form.redirect_to.as_deref() == Some("/bills") {
            "/bills"
        } else {
            "/dashboard"
        },
    ))
}

pub async fn people(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<DebtPeopleQuery>,
) -> HandlerResult<Html<String>> {
    render_people(&state, &dek, query, false).await
}

pub async fn advanced_people_search(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<DebtPeopleQuery>,
) -> HandlerResult<Html<String>> {
    render_people(&state, &dek, query, true).await
}

async fn render_people(
    state: &AppState,
    dek: &crypto::Dek,
    mut query: DebtPeopleQuery,
    advanced_search: bool,
) -> HandlerResult<Html<String>> {
    if !advanced_search {
        query.mode = "and".into();
        query.relationship.clear();
        query.min_receivable.clear();
        query.max_receivable.clear();
        query.min_payable.clear();
        query.max_payable.clear();
    }
    if !query.relationship.is_empty()
        && !matches!(
            query.relationship.as_str(),
            "receivable" | "payable" | "both" | "settled"
        )
    {
        query.relationship.clear();
    }
    let min_receivable = parse_optional_amount(&query.min_receivable, "最低应收")?;
    let max_receivable = parse_optional_amount(&query.max_receivable, "最高应收")?;
    let min_payable = parse_optional_amount(&query.min_payable, "最低应付")?;
    let max_payable = parse_optional_amount(&query.max_payable, "最高应付")?;
    if min_receivable
        .zip(max_receivable)
        .is_some_and(|(min, max)| min > max)
    {
        return Err(bad_request("最低应收不能大于最高应收"));
    }
    if min_payable
        .zip(max_payable)
        .is_some_and(|(min, max)| min > max)
    {
        return Err(bad_request("最低应付不能大于最高应付"));
    }
    let people = debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let records = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_currencies: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();
    let default_currency = currency::default_currency(state).await.map_err(err500)?;
    let rates = currency::RateTable::load(
        state,
        accounts
            .into_iter()
            .map(|account| account.currency)
            .collect::<Vec<_>>(),
        &default_currency,
        chrono::Local::now().date_naive(),
    )
    .await
    .map_err(err500)?;
    let mut outstanding: HashMap<i64, (i64, i64)> = HashMap::new();
    for record in records {
        let entry = outstanding.entry(record.person_id).or_default();
        let native_amount = crypto::decrypt_cents(&dek, &record.amount);
        let record_currency = account_currencies
            .get(&record.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        apply_outstanding(
            &record.kind,
            rates
                .convert(native_amount, record_currency)
                .map_err(err500)?,
            &mut entry.0,
            &mut entry.1,
        )?;
    }
    let rows = people
        .into_iter()
        .map(|person| {
            let (receivable, payable) = outstanding.get(&person.id).copied().unwrap_or_default();
            DebtPersonRow {
                id: person.id,
                name: crypto::decrypt_string(dek, &person.name),
                note: crypto::decrypt_string(dek, &person.note),
                receivable: currency::format(receivable, &default_currency),
                payable: currency::format(payable, &default_currency),
                receivable_cents: receivable,
                payable_cents: payable,
            }
        })
        .collect::<Vec<_>>();
    let keyword = query.keyword.trim().to_lowercase();
    let mode_or = query.mode == "or";
    let has_filters = !keyword.is_empty()
        || !query.relationship.is_empty()
        || min_receivable.is_some()
        || max_receivable.is_some()
        || min_payable.is_some()
        || max_payable.is_some();
    let rows = rows
        .into_iter()
        .filter(|row| {
            let mut conditions = Vec::new();
            if !keyword.is_empty() {
                conditions.push(
                    format!("{} {}", row.name, row.note)
                        .to_lowercase()
                        .contains(&keyword),
                );
            }
            if !query.relationship.is_empty() {
                conditions.push(match query.relationship.as_str() {
                    "receivable" => row.receivable_cents > 0,
                    "payable" => row.payable_cents > 0,
                    "both" => row.receivable_cents > 0 && row.payable_cents > 0,
                    "settled" => row.receivable_cents == 0 && row.payable_cents == 0,
                    _ => true,
                });
            }
            if min_receivable.is_some() || max_receivable.is_some() {
                conditions.push(
                    min_receivable.is_none_or(|min| row.receivable_cents >= min)
                        && max_receivable.is_none_or(|max| row.receivable_cents <= max),
                );
            }
            if min_payable.is_some() || max_payable.is_some() {
                conditions.push(
                    min_payable.is_none_or(|min| row.payable_cents >= min)
                        && max_payable.is_none_or(|max| row.payable_cents <= max),
                );
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
    let total_records = rows.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let rows = rows
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();
    let html = DebtPeopleTemplate {
        page_heading: if advanced_search {
            "借贷对象高级搜索"
        } else {
            "借贷对象"
        }
        .into(),
        advanced_search,
        search_action: if advanced_search {
            "/debt-people/search"
        } else {
            "/debt-people"
        }
        .into(),
        people: rows,
        default_currency,
        mode: if mode_or { "or" } else { "and" }.into(),
        keyword: query.keyword.clone(),
        relationship: query.relationship.clone(),
        min_receivable: query.min_receivable.clone(),
        max_receivable: query.max_receivable.clone(),
        min_payable: query.min_payable.clone(),
        max_payable: query.max_payable.clone(),
        has_filters,
        per_page: pagination.per_page,
        pagination: super::pagination_view(
            &pagination,
            total_records,
            if advanced_search {
                "/debt-people/search"
            } else {
                "/debt-people"
            },
            "人",
            [
                ("mode", query.mode.clone()),
                ("keyword", query.keyword.clone()),
                ("relationship", query.relationship.clone()),
                ("min_receivable", query.min_receivable.clone()),
                ("max_receivable", query.max_receivable.clone()),
                ("min_payable", query.min_payable.clone()),
                ("max_payable", query.max_payable.clone()),
            ],
        ),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_person_form() -> Html<String> {
    Html(
        DebtPersonFormTemplate {
            heading: "新增借贷对象".into(),
            action: "/debt-people".into(),
            name: String::new(),
            note: String::new(),
        }
        .render()
        .expect("模板渲染失败"),
    )
}

pub async fn create_person(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<DebtPersonFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad_request("姓名不能为空"));
    }
    debt_person::ActiveModel {
        name: Set(crypto::encrypt(&dek, form.name.trim().as_bytes())),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}

pub async fn edit_person_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let person = debt_person::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借贷对象不存在".into()))?;
    let html = DebtPersonFormTemplate {
        heading: "编辑借贷对象".into(),
        action: format!("/debt-people/{id}/edit"),
        name: crypto::decrypt_string(&dek, &person.name),
        note: crypto::decrypt_string(&dek, &person.note),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update_person(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DebtPersonFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad_request("姓名不能为空"));
    }
    let person = debt_person::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借贷对象不存在".into()))?;
    let mut active = person.into_active_model();
    active.name = Set(crypto::encrypt(&dek, form.name.trim().as_bytes()));
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}

pub async fn delete_person(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    if debt_person::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, "借贷对象不存在".into()));
    }
    let mut balance_changes: HashMap<i64, i64> = HashMap::new();
    for record in debt_record::Entity::find()
        .filter(debt_record::Column::PersonId.eq(id))
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let delta = account_delta(&record.kind, crypto::decrypt_cents(&dek, &record.amount))?
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?;
        let current = balance_changes
            .get(&record.account_id)
            .copied()
            .unwrap_or_default();
        balance_changes.insert(
            record.account_id,
            current
                .checked_add(delta)
                .ok_or_else(|| bad_request("余额超出范围"))?,
        );
    }
    for (account_id, delta) in balance_changes {
        super::accounts::ensure_balance_delta(&state, &dek, account_id, delta).await?;
    }
    debt_person::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}
