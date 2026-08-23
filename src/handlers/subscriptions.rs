use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::{Duration, Months, NaiveDateTime};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set, TransactionTrait};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{account, account_detail, bill, category, subscription},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const DISPLAY_FMT: &str = "%Y-%m-%d %H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct AccountOption {
    id: i64,
    name: String,
}

struct CategoryOption {
    name: String,
}

struct SubscriptionRow {
    id: i64,
    name: String,
    amount: String,
    category: String,
    period_label: String,
    expires_at: String,
    note: String,
    expired: bool,
    due_soon: bool,
    status: String,
    period: String,
    currency: String,
    expires_at_value: NaiveDateTime,
}

#[derive(Template)]
#[template(path = "subscriptions.html")]
struct SubscriptionsTemplate {
    page_heading: String,
    advanced_search: bool,
    search_action: String,
    subscriptions: Vec<SubscriptionRow>,
    accounts: Vec<AccountOption>,
    total_count: usize,
    due_soon_count: usize,
    expired_count: usize,
    mode: String,
    keyword: String,
    status: String,
    period: String,
    currency: String,
    category: String,
    start_date: String,
    end_date: String,
    has_filters: bool,
    categories: Vec<CategoryOption>,
    currencies: &'static [currency::CurrencyOption],
    page: usize,
    per_page: usize,
    total_pages: usize,
    total_records: usize,
}

#[derive(Template)]
#[template(path = "subscription_form.html")]
struct SubscriptionFormTemplate {
    heading: String,
    action: String,
    categories: Vec<CategoryOption>,
    name: String,
    amount: String,
    currency: String,
    currencies: &'static [currency::CurrencyOption],
    category: String,
    period: String,
    expires_at: String,
    note: String,
}

#[derive(Deserialize)]
pub struct SubscriptionFormData {
    name: String,
    amount: String,
    currency: String,
    category: String,
    period: String,
    expires_at: String,
    note: String,
}

#[derive(Deserialize)]
pub struct CreateExpenseFormData {
    account_id: i64,
}

#[derive(Default, Deserialize)]
pub struct SubscriptionsQuery {
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
    period: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
}

struct ParsedSubscription {
    name: String,
    amount: i64,
    currency: String,
    category: String,
    period: String,
    expires_at: NaiveDateTime,
    note: String,
}

fn parse_form(form: SubscriptionFormData) -> HandlerResult<ParsedSubscription> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(bad_request("订阅服务名不能为空"));
    }
    let amount_decimal = Decimal::from_str(form.amount.trim())
        .map_err(|_| bad_request("金额格式不正确"))?
        .round_dp(2);
    if amount_decimal <= Decimal::ZERO {
        return Err(bad_request("金额必须大于 0"));
    }
    let amount = (amount_decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("金额超出范围"))?;
    if !currency::valid(&form.currency) {
        return Err(bad_request("订阅货币无效"));
    }
    let category = form.category.trim();
    if category.is_empty() {
        return Err(bad_request("支出分类不能为空"));
    }
    if !valid_period(&form.period) {
        return Err(bad_request("订阅周期无效"));
    }
    let expires_at = NaiveDateTime::parse_from_str(form.expires_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("到期时间格式不正确"))?;
    Ok(ParsedSubscription {
        name: name.into(),
        amount,
        currency: form.currency,
        category: category.into(),
        period: form.period,
        expires_at,
        note: form.note.trim().into(),
    })
}

fn valid_period(period: &str) -> bool {
    matches!(period, "day" | "week" | "month" | "quarter" | "year")
}

fn period_label(period: &str) -> &'static str {
    match period {
        "day" => "每日",
        "week" => "每周",
        "month" => "每月",
        "quarter" => "每季",
        "year" => "每年",
        _ => "未知周期",
    }
}

fn renewed_expiry(
    expires_at: NaiveDateTime,
    period: &str,
    now: NaiveDateTime,
) -> HandlerResult<NaiveDateTime> {
    let base = expires_at.max(now);
    let renewed = match period {
        "day" => base.checked_add_signed(Duration::days(1)),
        "week" => base.checked_add_signed(Duration::weeks(1)),
        "month" => base.checked_add_months(Months::new(1)),
        "quarter" => base.checked_add_months(Months::new(3)),
        "year" => base.checked_add_months(Months::new(12)),
        _ => return Err(bad_request("订阅周期无效")),
    };
    renewed.ok_or_else(|| bad_request("续期后的到期时间超出范围"))
}

async fn expense_categories(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<Vec<CategoryOption>> {
    Ok(category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|category| category.kind == "expense")
        .map(|category| CategoryOption {
            name: crypto::decrypt_string(dek, &category.name),
        })
        .collect())
}

async fn account_options(state: &AppState, dek: &crypto::Dek) -> HandlerResult<Vec<AccountOption>> {
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    Ok(account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|account| AccountOption {
            id: account.id,
            name: super::bills::account_display_name(dek, &account, details.get(&account.id)),
        })
        .collect())
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<SubscriptionsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, query, false).await
}

pub async fn advanced_search(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<SubscriptionsQuery>,
) -> HandlerResult<Html<String>> {
    render_list(&state, &dek, query, true).await
}

async fn render_list(
    state: &AppState,
    dek: &crypto::Dek,
    mut query: SubscriptionsQuery,
    advanced_search: bool,
) -> HandlerResult<Html<String>> {
    if !advanced_search {
        query.mode = "and".into();
        query.status.clear();
        query.period.clear();
        query.currency.clear();
        query.category.clear();
        query.start_date.clear();
        query.end_date.clear();
    }
    let start_date = if query.start_date.trim().is_empty() {
        None
    } else {
        Some(
            chrono::NaiveDate::parse_from_str(query.start_date.trim(), "%Y-%m-%d")
                .map_err(|_| bad_request("开始日期格式不正确"))?,
        )
    };
    let end_date = if query.end_date.trim().is_empty() {
        None
    } else {
        Some(
            chrono::NaiveDate::parse_from_str(query.end_date.trim(), "%Y-%m-%d")
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
        && !matches!(query.status.as_str(), "active" | "due_soon" | "expired")
    {
        query.status.clear();
    }
    if !query.period.is_empty() && !valid_period(&query.period) {
        query.period.clear();
    }
    if !query.currency.is_empty() && !currency::valid(&query.currency) {
        query.currency.clear();
    }
    let now = chrono::Local::now().naive_local();
    let soon = now + Duration::days(7);
    let keyword = query.keyword.trim().to_lowercase();
    let category_filter = query.category.trim().to_lowercase();
    let mode_or = query.mode == "or";
    let has_filters = !keyword.is_empty()
        || !query.status.is_empty()
        || !query.period.is_empty()
        || !query.currency.is_empty()
        || !category_filter.is_empty()
        || start_date.is_some()
        || end_date.is_some();
    let subscriptions = subscription::Entity::find()
        .order_by_asc(subscription::Column::ExpiresAt)
        .order_by_asc(subscription::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|subscription| {
            let expired = subscription.expires_at < now;
            let due_soon = !expired && subscription.expires_at <= soon;
            let status = if expired {
                "已到期"
            } else if due_soon {
                "7 天内到期"
            } else {
                "有效"
            };
            let category = crypto::decrypt_string(dek, &subscription.category);
            SubscriptionRow {
                id: subscription.id,
                name: crypto::decrypt_string(dek, &subscription.name),
                amount: currency::format(
                    crypto::decrypt_cents(dek, &subscription.amount),
                    &subscription.currency,
                ),
                category,
                period_label: period_label(&subscription.period).into(),
                expires_at: subscription.expires_at.format(DISPLAY_FMT).to_string(),
                note: crypto::decrypt_string(dek, &subscription.note),
                expired,
                due_soon,
                status: status.into(),
                period: subscription.period,
                currency: subscription.currency,
                expires_at_value: subscription.expires_at,
            }
        })
        .filter(|row| {
            let mut conditions = Vec::new();
            if !keyword.is_empty() {
                conditions.push(
                    format!(
                        "{} {} {} {} {} {}",
                        row.name,
                        row.category,
                        row.note,
                        row.period_label,
                        row.currency,
                        row.status
                    )
                    .to_lowercase()
                    .contains(&keyword),
                );
            }
            if !query.status.is_empty() {
                conditions.push(match query.status.as_str() {
                    "expired" => row.expired,
                    "due_soon" => row.due_soon,
                    "active" => !row.expired && !row.due_soon,
                    _ => true,
                });
            }
            if !query.period.is_empty() {
                conditions.push(row.period == query.period);
            }
            if !query.currency.is_empty() {
                conditions.push(row.currency == query.currency);
            }
            if !category_filter.is_empty() {
                conditions.push(row.category.to_lowercase() == category_filter);
            }
            if start_date.is_some() || end_date.is_some() {
                let date = row.expires_at_value.date();
                conditions.push(
                    start_date.is_none_or(|start| date >= start)
                        && end_date.is_none_or(|end| date <= end),
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
    let total_records = subscriptions.len();
    let expired_count = subscriptions.iter().filter(|row| row.expired).count();
    let due_soon_count = subscriptions.iter().filter(|row| row.due_soon).count();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    let subscriptions = subscriptions
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();
    let html = SubscriptionsTemplate {
        page_heading: if advanced_search {
            "订阅高级搜索"
        } else {
            "订阅服务"
        }
        .into(),
        advanced_search,
        search_action: if advanced_search {
            "/subscriptions/search"
        } else {
            "/subscriptions"
        }
        .into(),
        total_count: total_records,
        subscriptions,
        accounts: account_options(state, dek).await?,
        due_soon_count,
        expired_count,
        mode: if mode_or { "or" } else { "and" }.into(),
        keyword: query.keyword,
        status: query.status,
        period: query.period,
        currency: query.currency,
        category: query.category,
        start_date: query.start_date,
        end_date: query.end_date,
        has_filters,
        categories: expense_categories(state, dek).await?,
        currencies: currency::CURRENCIES,
        page: pagination.page,
        per_page: pagination.per_page,
        total_pages: pagination.total_pages,
        total_records,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let html = SubscriptionFormTemplate {
        heading: "新增订阅".into(),
        action: "/subscriptions".into(),
        categories: expense_categories(&state, &dek).await?,
        name: String::new(),
        amount: String::new(),
        currency: currency::default_currency(&state).await.map_err(err500)?,
        currencies: currency::CURRENCIES,
        category: String::new(),
        period: "month".into(),
        expires_at: chrono::Local::now()
            .naive_local()
            .format(TIME_FMT)
            .to_string(),
        note: String::new(),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<SubscriptionFormData>,
) -> HandlerResult<Redirect> {
    let parsed = parse_form(form)?;
    super::bills::ensure_category_exists(&state, &dek, "expense", &parsed.category).await?;
    subscription::ActiveModel {
        name: Set(crypto::encrypt(&dek, parsed.name.as_bytes())),
        amount: Set(crypto::encrypt_cents(&dek, parsed.amount)),
        currency: Set(parsed.currency),
        category: Set(crypto::encrypt(&dek, parsed.category.as_bytes())),
        period: Set(parsed.period),
        expires_at: Set(parsed.expires_at),
        note: Set(crypto::encrypt(&dek, parsed.note.as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/subscriptions"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let subscription = subscription::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "订阅不存在".into()))?;
    let html = SubscriptionFormTemplate {
        heading: "编辑订阅".into(),
        action: format!("/subscriptions/{id}/edit"),
        categories: expense_categories(&state, &dek).await?,
        name: crypto::decrypt_string(&dek, &subscription.name),
        amount: super::fmt_cents(crypto::decrypt_cents(&dek, &subscription.amount)),
        currency: subscription.currency,
        currencies: currency::CURRENCIES,
        category: crypto::decrypt_string(&dek, &subscription.category),
        period: subscription.period,
        expires_at: subscription.expires_at.format(TIME_FMT).to_string(),
        note: crypto::decrypt_string(&dek, &subscription.note),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<SubscriptionFormData>,
) -> HandlerResult<Redirect> {
    let parsed = parse_form(form)?;
    super::bills::ensure_category_exists(&state, &dek, "expense", &parsed.category).await?;
    let subscription = subscription::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "订阅不存在".into()))?;
    let mut active = subscription.into_active_model();
    active.name = Set(crypto::encrypt(&dek, parsed.name.as_bytes()));
    active.amount = Set(crypto::encrypt_cents(&dek, parsed.amount));
    active.currency = Set(parsed.currency);
    active.category = Set(crypto::encrypt(&dek, parsed.category.as_bytes()));
    active.period = Set(parsed.period);
    active.expires_at = Set(parsed.expires_at);
    active.note = Set(crypto::encrypt(&dek, parsed.note.as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/subscriptions"))
}

pub async fn create_expense(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<CreateExpenseFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let subscription = subscription::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "订阅不存在".into()))?;
    let subscription_amount = crypto::decrypt_cents(&dek, &subscription.amount);
    let account = account::Entity::find_by_id(form.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("付款账户不存在"))?;
    let now = chrono::Local::now().naive_local();
    let amount = currency::convert_cents(
        &state,
        subscription_amount,
        &subscription.currency,
        &account.currency,
        now.date(),
    )
    .await
    .map_err(err500)?;
    let category = crypto::decrypt_string(&dek, &subscription.category);
    let is_food = super::bills::category_is_food(&state, &dek, "expense", &category).await?;
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        form.account_id,
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?,
    )
    .await?;
    let name = crypto::decrypt_string(&dek, &subscription.name);
    let subscription_note = crypto::decrypt_string(&dek, &subscription.note);
    let note = if subscription_note.is_empty() {
        format!("订阅：{name}")
    } else {
        format!("订阅：{name} · {subscription_note}")
    };
    let next_expiry = renewed_expiry(subscription.expires_at, &subscription.period, now)?;
    let transaction = state.db.begin().await.map_err(err500)?;
    bill::ActiveModel {
        account_id: Set(form.account_id),
        kind: Set("expense".into()),
        amount: Set(crypto::encrypt_cents(&dek, amount)),
        category: Set(crypto::encrypt(&dek, category.as_bytes())),
        is_food: Set(is_food),
        note: Set(crypto::encrypt(&dek, note.as_bytes())),
        happened_at: Set(now),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    let mut active = subscription.into_active_model();
    active.expires_at = Set(next_expiry);
    active.update(&transaction).await.map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to("/subscriptions"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    subscription::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/subscriptions"))
}
