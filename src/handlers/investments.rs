use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form, Json,
};
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto,
    entity::{
        account, bill, category, investment_execution, market_closed_day, recurring_investment,
        transfer,
    },
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

fn china_today() -> NaiveDate {
    (chrono::Utc::now().naive_utc() + Duration::hours(8)).date()
}

fn parse_amount(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("定投金额格式不正确"))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request("定投金额必须大于 0"));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("定投金额超出范围"))
}

fn parse_fee_rate_bps(value: &str) -> HandlerResult<i64> {
    if value.trim().is_empty() {
        return Ok(0);
    }
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("手续费率格式不正确"))?
        .round_dp(2);
    if decimal < Decimal::ZERO || decimal > Decimal::from(100) {
        return Err(bad_request("手续费率必须位于 0% 到 100% 之间"));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("手续费率超出范围"))
}

fn format_fee_rate(bps: i64) -> String {
    format!("{}%", Decimal::new(bps, 2).normalize())
}

fn calculate_fee(amount: i64, fee_rate_bps: i64) -> HandlerResult<i64> {
    let numerator = i128::from(amount)
        .checked_mul(i128::from(fee_rate_bps))
        .ok_or_else(|| bad_request("手续费计算超出范围"))?;
    let rounded = numerator
        .checked_add(5_000)
        .ok_or_else(|| bad_request("手续费计算超出范围"))?
        / 10_000;
    i64::try_from(rounded).map_err(|_| bad_request("手续费计算超出范围"))
}

async fn is_trading_day(state: &AppState, date: NaiveDate) -> HandlerResult<bool> {
    if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
        return Ok(false);
    }
    Ok(market_closed_day::Entity::find_by_id(date)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none())
}

async fn next_trading_day(state: &AppState, mut date: NaiveDate) -> HandlerResult<NaiveDate> {
    for _ in 0..=370 {
        if is_trading_day(state, date).await? {
            return Ok(date);
        }
        date = date
            .succ_opt()
            .ok_or_else(|| bad_request("下一交易日超出支持范围"))?;
    }
    Err(bad_request("未来一年内没有可用的中国大陆交易日"))
}

#[derive(Clone)]
struct AccountOption {
    id: i64,
    name: String,
    kind: String,
    currency: String,
}

struct PlanRow {
    id: i64,
    name: String,
    from_account: String,
    fund_account: String,
    amount: String,
    fee_rate: String,
    currency: String,
    start_date: String,
    next_trade_date: String,
    active: bool,
    due: bool,
    note: String,
}

struct ExecutionRow {
    plan_name: String,
    trade_date: String,
    from_account: String,
    fund_account: String,
    amount: String,
    fee: String,
}

struct ClosedDayRow {
    date: String,
    name: String,
}

#[derive(Template)]
#[template(path = "investments.html")]
struct InvestmentsTemplate {
    plans: Vec<PlanRow>,
    executions: Vec<ExecutionRow>,
    custom_closed_days: Vec<ClosedDayRow>,
    keyword: String,
    status: String,
    per_page: usize,
    pagination: super::PaginationView,
    due_count: usize,
    active_count: usize,
    today: String,
    calendar_warning: bool,
}

#[derive(Template)]
#[template(path = "investment_form.html")]
struct InvestmentFormTemplate {
    heading: String,
    action: String,
    name: String,
    amount: String,
    fee_rate: String,
    from_account_id: i64,
    fund_account_id: i64,
    start_date: String,
    note: String,
    active: bool,
    min_start_date: String,
    source_accounts: Vec<AccountOption>,
    fund_accounts: Vec<AccountOption>,
}

#[derive(Default, Deserialize)]
pub struct InvestmentsQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize)]
pub struct InvestmentFormData {
    name: String,
    amount: String,
    #[serde(default)]
    fee_rate: String,
    from_account_id: i64,
    fund_account_id: i64,
    start_date: String,
    note: String,
    #[serde(default)]
    active: Option<String>,
}

struct ParsedInvestment {
    name: String,
    amount: i64,
    fee_rate_bps: i64,
    from_account: account::Model,
    fund_account: account::Model,
    start_date: NaiveDate,
    note: String,
    active: bool,
}

#[derive(Deserialize)]
pub struct ClosedDayFormData {
    date: String,
    name: String,
}

#[derive(Serialize)]
pub struct RunDueResponse {
    ok: bool,
    executed: usize,
    failures: Vec<String>,
    message: String,
}

async fn account_options(state: &AppState, dek: &crypto::Dek) -> HandlerResult<Vec<AccountOption>> {
    Ok(account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|account| AccountOption {
            id: account.id,
            name: crypto::decrypt_string(dek, &account.name),
            kind: account.kind,
            currency: account.currency,
        })
        .collect())
}

async fn parse_form(state: &AppState, form: InvestmentFormData) -> HandlerResult<ParsedInvestment> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(bad_request("定投计划名不能为空"));
    }
    if form.from_account_id == form.fund_account_id {
        return Err(bad_request("扣款账户和基金账户不能相同"));
    }
    let from_account = account::Entity::find_by_id(form.from_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("扣款账户不存在"))?;
    if matches!(from_account.kind.as_str(), "credit_card" | "credit_service") {
        return Err(bad_request("信用卡和信贷服务不能作为定投扣款账户"));
    }
    let fund_account = account::Entity::find_by_id(form.fund_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("基金账户不存在"))?;
    if fund_account.kind != "investment" {
        return Err(bad_request("定投的固定基金账户必须是投资账户"));
    }
    if from_account.currency != fund_account.currency {
        return Err(bad_request("定投扣款账户和基金账户必须使用相同货币"));
    }
    let start_date = NaiveDate::parse_from_str(form.start_date.trim(), "%Y-%m-%d")
        .map_err(|_| bad_request("开始日期格式不正确"))?;
    Ok(ParsedInvestment {
        name: name.into(),
        amount: parse_amount(&form.amount)?,
        fee_rate_bps: parse_fee_rate_bps(&form.fee_rate)?,
        from_account,
        fund_account,
        start_date,
        note: form.note.trim().into(),
        active: form.active.is_some(),
    })
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(mut query): Query<InvestmentsQuery>,
) -> HandlerResult<Html<String>> {
    if !matches!(query.status.as_str(), "" | "active" | "paused" | "due") {
        query.status.clear();
    }
    let today = china_today();
    let accounts = account_options(&state, &dek).await?;
    let account_names = accounts
        .iter()
        .map(|account| (account.id, account.name.clone()))
        .collect::<HashMap<_, _>>();
    let account_currencies = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect::<HashMap<_, _>>();
    let keyword = query.keyword.trim().to_lowercase();
    let plan_rows = recurring_investment::Entity::find()
        .order_by_asc(recurring_investment::Column::NextTradeDate)
        .order_by_asc(recurring_investment::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|plan| {
            let name = crypto::decrypt_string(&dek, &plan.name);
            let note = crypto::decrypt_string(&dek, &plan.note);
            let from_account = account_names
                .get(&plan.from_account_id)
                .cloned()
                .unwrap_or_else(|| "已删除账户".into());
            let fund_account = account_names
                .get(&plan.fund_account_id)
                .cloned()
                .unwrap_or_else(|| "已删除基金账户".into());
            let currency = account_currencies
                .get(&plan.from_account_id)
                .cloned()
                .unwrap_or_default();
            PlanRow {
                id: plan.id,
                name,
                from_account,
                fund_account,
                amount: crate::currency::format(
                    crypto::decrypt_cents(&dek, &plan.amount),
                    &currency,
                ),
                fee_rate: format_fee_rate(crypto::decrypt_cents(&dek, &plan.fee_rate_bps)),
                currency,
                start_date: plan.start_date.format("%Y-%m-%d").to_string(),
                next_trade_date: plan.next_trade_date.format("%Y-%m-%d").to_string(),
                active: plan.active,
                due: plan.active && plan.next_trade_date <= today,
                note,
            }
        })
        .collect::<Vec<_>>();
    let active_count = plan_rows.iter().filter(|plan| plan.active).count();
    let due_count = plan_rows.iter().filter(|plan| plan.due).count();
    let mut plans = plan_rows
        .into_iter()
        .filter(|row| {
            let matches_keyword = keyword.is_empty()
                || format!(
                    "{} {} {} {} {}",
                    row.name, row.from_account, row.fund_account, row.note, row.currency
                )
                .to_lowercase()
                .contains(&keyword);
            let matches_status = match query.status.as_str() {
                "active" => row.active,
                "paused" => !row.active,
                "due" => row.due,
                _ => true,
            };
            matches_keyword && matches_status
        })
        .collect::<Vec<_>>();
    let total_records = plans.len();
    let pagination = super::pagination(total_records, query.page, query.per_page);
    plans = plans
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .collect();

    let all_plans = recurring_investment::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|plan| (plan.id, crypto::decrypt_string(&dek, &plan.name)))
        .collect::<HashMap<_, _>>();
    let transfers = transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|transfer| (transfer.id, transfer))
        .collect::<HashMap<_, _>>();
    let bills = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| (bill.id, bill))
        .collect::<HashMap<_, _>>();
    let executions = investment_execution::Entity::find()
        .order_by_desc(investment_execution::Column::TradeDate)
        .order_by_desc(investment_execution::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter_map(|execution| {
            let transfer = transfers.get(&execution.transfer_id?)?;
            let currency = account_currencies
                .get(&transfer.from_account_id)
                .cloned()
                .unwrap_or_default();
            let fee = execution
                .fee_bill_id
                .and_then(|id| bills.get(&id))
                .map(|bill| crypto::decrypt_cents(&dek, &bill.amount))
                .unwrap_or_default();
            Some(ExecutionRow {
                plan_name: all_plans
                    .get(&execution.plan_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除计划".into()),
                trade_date: execution.trade_date.format("%Y-%m-%d").to_string(),
                from_account: account_names
                    .get(&transfer.from_account_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除账户".into()),
                fund_account: account_names
                    .get(&transfer.to_account_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除基金账户".into()),
                amount: crate::currency::format(
                    crypto::decrypt_cents(&dek, &transfer.amount),
                    &currency,
                ),
                fee: crate::currency::format(fee, &currency),
            })
        })
        .take(50)
        .collect();
    let custom_closed_days = market_closed_day::Entity::find()
        .filter(market_closed_day::Column::Source.eq("user"))
        .order_by_asc(market_closed_day::Column::Date)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|day| ClosedDayRow {
            date: day.date.format("%Y-%m-%d").to_string(),
            name: day.name,
        })
        .collect();
    let html = InvestmentsTemplate {
        plans,
        executions,
        custom_closed_days,
        keyword: query.keyword.clone(),
        status: query.status.clone(),
        per_page: pagination.per_page,
        pagination: super::pagination_view(
            &pagination,
            total_records,
            "/investments",
            "个定投计划",
            [("keyword", query.keyword), ("status", query.status)],
        ),
        due_count,
        active_count,
        today: today.format("%Y-%m-%d").to_string(),
        calendar_warning: today.year() > 2026,
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
    let source_accounts = accounts
        .iter()
        .filter(|account| !matches!(account.kind.as_str(), "credit_card" | "credit_service"))
        .cloned()
        .collect();
    let fund_accounts = accounts
        .into_iter()
        .filter(|account| account.kind == "investment")
        .collect();
    let today = china_today().format("%Y-%m-%d").to_string();
    let html = InvestmentFormTemplate {
        heading: "新增每日定投".into(),
        action: "/investments".into(),
        name: String::new(),
        amount: String::new(),
        fee_rate: "0.00".into(),
        from_account_id: 0,
        fund_account_id: 0,
        start_date: today.clone(),
        note: String::new(),
        active: true,
        min_start_date: today,
        source_accounts,
        fund_accounts,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<InvestmentFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let parsed = parse_form(&state, form).await?;
    if parsed.start_date < china_today() {
        return Err(bad_request("新定投计划不能从过去日期开始"));
    }
    let next_trade_date = next_trading_day(&state, parsed.start_date).await?;
    recurring_investment::ActiveModel {
        name: Set(crypto::encrypt(&dek, parsed.name.as_bytes())),
        from_account_id: Set(parsed.from_account.id),
        fund_account_id: Set(parsed.fund_account.id),
        amount: Set(crypto::encrypt_cents(&dek, parsed.amount)),
        fee_rate_bps: Set(crypto::encrypt_cents(&dek, parsed.fee_rate_bps)),
        start_date: Set(parsed.start_date),
        next_trade_date: Set(next_trade_date),
        active: Set(parsed.active),
        note: Set(crypto::encrypt(&dek, parsed.note.as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/investments"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let plan = recurring_investment::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "定投计划不存在".into()))?;
    let accounts = account_options(&state, &dek).await?;
    let source_accounts = accounts
        .iter()
        .filter(|account| !matches!(account.kind.as_str(), "credit_card" | "credit_service"))
        .cloned()
        .collect();
    let fund_accounts = accounts
        .into_iter()
        .filter(|account| account.kind == "investment")
        .collect();
    let start_date = plan.start_date.format("%Y-%m-%d").to_string();
    let html = InvestmentFormTemplate {
        heading: "编辑每日定投".into(),
        action: format!("/investments/{id}/edit"),
        name: crypto::decrypt_string(&dek, &plan.name),
        amount: super::fmt_cents(crypto::decrypt_cents(&dek, &plan.amount)),
        fee_rate: super::fmt_cents(crypto::decrypt_cents(&dek, &plan.fee_rate_bps)),
        from_account_id: plan.from_account_id,
        fund_account_id: plan.fund_account_id,
        start_date: start_date.clone(),
        note: crypto::decrypt_string(&dek, &plan.note),
        active: plan.active,
        min_start_date: start_date,
        source_accounts,
        fund_accounts,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<InvestmentFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let parsed = parse_form(&state, form).await?;
    let plan = recurring_investment::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "定投计划不存在".into()))?;
    let reset_schedule = !plan.active && parsed.active || parsed.start_date > plan.next_trade_date;
    let next_trade_date = if reset_schedule {
        next_trading_day(&state, parsed.start_date.max(china_today())).await?
    } else {
        plan.next_trade_date
    };
    let mut active = plan.into_active_model();
    active.name = Set(crypto::encrypt(&dek, parsed.name.as_bytes()));
    active.from_account_id = Set(parsed.from_account.id);
    active.fund_account_id = Set(parsed.fund_account.id);
    active.amount = Set(crypto::encrypt_cents(&dek, parsed.amount));
    active.fee_rate_bps = Set(crypto::encrypt_cents(&dek, parsed.fee_rate_bps));
    active.start_date = Set(parsed.start_date);
    active.next_trade_date = Set(next_trade_date);
    active.active = Set(parsed.active);
    active.note = Set(crypto::encrypt(&dek, parsed.note.as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/investments"))
}

pub async fn toggle(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let plan = recurring_investment::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "定投计划不存在".into()))?;
    let mut active = plan.into_active_model();
    if active.active.as_ref() == &true {
        active.active = Set(false);
    } else {
        let start_date = *active.start_date.as_ref();
        active.active = Set(true);
        active.next_trade_date =
            Set(next_trading_day(&state, start_date.max(china_today())).await?);
    }
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/investments"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let result = recurring_investment::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    if result.rows_affected == 0 {
        return Err((StatusCode::NOT_FOUND, "定投计划不存在".into()));
    }
    Ok(Redirect::to("/investments"))
}

async fn ensure_fee_category(state: &AppState, dek: &crypto::Dek) -> HandlerResult<()> {
    let exists = category::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .any(|item| {
            item.kind == "expense" && crypto::decrypt_string(dek, &item.name) == "投资手续费"
        });
    if !exists {
        category::ActiveModel {
            kind: Set("expense".into()),
            name: Set(crypto::encrypt(dek, "投资手续费".as_bytes())),
            is_food: Set(false),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(())
}

async fn execute_plan_day(
    state: &AppState,
    dek: &crypto::Dek,
    plan: &recurring_investment::Model,
    trade_date: NaiveDate,
) -> HandlerResult<NaiveDate> {
    if !is_trading_day(state, trade_date).await? {
        return Err(bad_request(format!("{trade_date} 不是中国大陆交易日")));
    }
    if investment_execution::Entity::find()
        .filter(investment_execution::Column::PlanId.eq(plan.id))
        .filter(investment_execution::Column::TradeDate.eq(trade_date))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        let next_date = next_trading_day(
            state,
            trade_date
                .succ_opt()
                .ok_or_else(|| bad_request("下一交易日超出范围"))?,
        )
        .await?;
        let mut active = plan.clone().into_active_model();
        active.next_trade_date = Set(next_date);
        active.update(&state.db).await.map_err(err500)?;
        return Ok(next_date);
    }
    let from_account = account::Entity::find_by_id(plan.from_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("定投扣款账户不存在"))?;
    let fund_account = account::Entity::find_by_id(plan.fund_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("定投基金账户不存在"))?;
    if matches!(from_account.kind.as_str(), "credit_card" | "credit_service") {
        return Err(bad_request("信用卡和信贷服务不能作为定投扣款账户"));
    }
    if fund_account.kind != "investment" {
        return Err(bad_request("定投基金账户不再是投资账户，请先修改计划"));
    }
    if from_account.currency != fund_account.currency {
        return Err(bad_request("定投两端账户货币不一致，请先修改计划"));
    }
    let amount = crypto::decrypt_cents(dek, &plan.amount);
    let fee_rate_bps = crypto::decrypt_cents(dek, &plan.fee_rate_bps);
    let fee = calculate_fee(amount, fee_rate_bps)?;
    let total_debit = amount
        .checked_add(fee)
        .ok_or_else(|| bad_request("定投本金和手续费合计超出范围"))?;
    super::accounts::ensure_balance_delta(
        state,
        dek,
        plan.from_account_id,
        total_debit
            .checked_neg()
            .ok_or_else(|| bad_request("定投金额超出范围"))?,
    )
    .await?;
    if fee > 0 {
        ensure_fee_category(state, dek).await?;
    }
    let next_date = next_trading_day(
        state,
        trade_date
            .succ_opt()
            .ok_or_else(|| bad_request("下一交易日超出范围"))?,
    )
    .await?;
    // 交易日流水记在北京时间 15:00，对应 UTC 07:00；数据库仍只存 UTC。
    let happened_at = trade_date
        .and_hms_opt(7, 0, 0)
        .ok_or_else(|| bad_request("定投交易时间无效"))?;
    let plan_name = crypto::decrypt_string(dek, &plan.name);
    let transaction = state.db.begin().await.map_err(err500)?;
    let transfer = transfer::ActiveModel {
        from_account_id: Set(plan.from_account_id),
        to_account_id: Set(plan.fund_account_id),
        amount: Set(crypto::encrypt_cents(dek, amount)),
        to_amount: Set(crypto::encrypt_cents(dek, amount)),
        note: Set(crypto::encrypt(
            dek,
            format!("每日定投 · {plan_name}").as_bytes(),
        )),
        happened_at: Set(happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    let fee_bill_id = if fee > 0 {
        Some(
            bill::ActiveModel {
                account_id: Set(plan.from_account_id),
                kind: Set("expense".into()),
                amount: Set(crypto::encrypt_cents(dek, fee)),
                category: Set(crypto::encrypt(dek, "投资手续费".as_bytes())),
                is_food: Set(false),
                note: Set(crypto::encrypt(
                    dek,
                    format!("每日定投手续费 · {plan_name}").as_bytes(),
                )),
                happened_at: Set(happened_at),
                created_at: Set(chrono::Utc::now()),
                ..Default::default()
            }
            .insert(&transaction)
            .await
            .map_err(err500)?
            .id,
        )
    } else {
        None
    };
    investment_execution::ActiveModel {
        plan_id: Set(plan.id),
        trade_date: Set(trade_date),
        transfer_id: Set(Some(transfer.id)),
        fee_bill_id: Set(fee_bill_id),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    let mut active = plan.clone().into_active_model();
    active.next_trade_date = Set(next_date);
    active.update(&transaction).await.map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(next_date)
}

pub async fn run_due(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Json<RunDueResponse>> {
    let _balance_guard = state.balance_writes.lock().await;
    let today = china_today();
    let plans = recurring_investment::Entity::find()
        .filter(recurring_investment::Column::Active.eq(true))
        .filter(recurring_investment::Column::NextTradeDate.lte(today))
        .order_by_asc(recurring_investment::Column::NextTradeDate)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut executed = 0usize;
    let mut failures = Vec::new();
    for mut plan in plans {
        let plan_name = crypto::decrypt_string(&dek, &plan.name);
        let mut per_plan = 0usize;
        while plan.next_trade_date <= today && per_plan < 370 {
            let trade_date = plan.next_trade_date;
            if !is_trading_day(&state, trade_date).await? {
                let next_date = next_trading_day(
                    &state,
                    trade_date
                        .succ_opt()
                        .ok_or_else(|| bad_request("下一交易日超出范围"))?,
                )
                .await?;
                let mut active = plan.clone().into_active_model();
                active.next_trade_date = Set(next_date);
                active.update(&state.db).await.map_err(err500)?;
                plan.next_trade_date = next_date;
                continue;
            }
            match execute_plan_day(&state, &dek, &plan, trade_date).await {
                Ok(next_date) => {
                    plan.next_trade_date = next_date;
                    executed += 1;
                    per_plan += 1;
                }
                Err((_, message)) => {
                    failures.push(format!("{plan_name}（{trade_date}）：{message}"));
                    break;
                }
            }
        }
        if per_plan >= 370 && plan.next_trade_date <= today {
            failures.push(format!("{plan_name}：待执行交易日过多，请再次执行"));
        }
    }
    let ok = failures.is_empty();
    let message = if executed == 0 && ok {
        "当前没有待执行的定投".into()
    } else if ok {
        format!("已完成 {executed} 笔定投")
    } else {
        format!("已完成 {executed} 笔定投，{} 个计划未完成", failures.len())
    };
    Ok(Json(RunDueResponse {
        ok,
        executed,
        failures,
        message,
    }))
}

pub async fn create_closed_day(
    State(state): State<AppState>,
    Form(form): Form<ClosedDayFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let date = NaiveDate::parse_from_str(form.date.trim(), "%Y-%m-%d")
        .map_err(|_| bad_request("休市日期格式不正确"))?;
    let name = form.name.trim();
    if name.is_empty() {
        return Err(bad_request("休市原因不能为空"));
    }
    if market_closed_day::Entity::find_by_id(date)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request("该日期已经登记为休市日"));
    }
    market_closed_day::ActiveModel {
        date: Set(date),
        name: Set(name.into()),
        source: Set("user".into()),
        created_at: Set(chrono::Utc::now()),
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/investments"))
}

pub async fn delete_closed_day(
    State(state): State<AppState>,
    Path(date): Path<String>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
        .map_err(|_| bad_request("休市日期格式不正确"))?;
    let day = market_closed_day::Entity::find_by_id(date)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "休市日不存在".into()))?;
    if day.source != "user" {
        return Err(bad_request("内置的官方休市日不能删除"));
    }
    market_closed_day::Entity::delete_by_id(date)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/investments"))
}
