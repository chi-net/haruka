use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form, Json,
};
use chrono::{Duration, Months, NaiveDateTime};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto, currency,
    entity::{account, account_detail, savings_project, transfer},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

#[derive(Clone)]
struct AccountOption {
    id: i64,
    name: String,
    currency: String,
}

struct ProjectRow {
    id: i64,
    name: String,
    account_name: String,
    currency: String,
    balance: String,
    auto_label: String,
    next_run_value: String,
    note: String,
}

#[derive(Template)]
#[template(path = "savings.html")]
struct SavingsTemplate {
    projects: Vec<ProjectRow>,
    funding_accounts: Vec<AccountOption>,
    has_projects: bool,
}

#[derive(Template)]
#[template(path = "savings_form.html")]
struct SavingsFormTemplate {
    heading: String,
    action: String,
    name: String,
    account_name: String,
    account_id: i64,
    note: String,
    auto_enabled: bool,
    source_account_id: i64,
    amount: String,
    cycle: String,
    next_run_at: String,
    savings_accounts: Vec<AccountOption>,
    funding_accounts: Vec<AccountOption>,
    edit: bool,
}

#[derive(Deserialize)]
pub struct SavingsFormData {
    name: String,
    account_id: i64,
    #[serde(default)]
    note: String,
    #[serde(default)]
    auto_enabled: bool,
    #[serde(default)]
    source_account_id: i64,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    cycle: String,
    #[serde(default)]
    next_run_at: String,
}

#[derive(Deserialize)]
pub struct DepositForm {
    from_account_id: i64,
    amount: String,
}

#[derive(Deserialize)]
pub struct WithdrawForm {
    to_account_id: i64,
    amount: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    confirm_withdrawal: bool,
}

#[derive(Serialize)]
pub struct RunDueResponse {
    ok: bool,
    executed: usize,
    failures: Vec<String>,
    message: String,
}

fn is_eligible(account: &account::Model) -> bool {
    matches!(
        account.kind.as_str(),
        "payment" | "bank" | "stored_value" | "other"
    )
}

pub(crate) async fn is_savings_account_on<C: ConnectionTrait>(
    db: &C,
    account_id: i64,
) -> HandlerResult<bool> {
    Ok(savings_project::Entity::find()
        .filter(savings_project::Column::AccountId.eq(account_id))
        .one(db)
        .await
        .map_err(err500)?
        .is_some())
}

fn parse_amount(value: &str, label: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request(&format!("{label}格式不正确")))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request(&format!("{label}必须大于 0")));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request(&format!("{label}超出范围")))
}

fn valid_cycle(cycle: &str) -> bool {
    matches!(cycle, "day" | "week" | "month")
}

fn cycle_label(cycle: &str) -> &'static str {
    match cycle {
        "day" => "每日",
        "week" => "每周",
        "month" => "每月",
        _ => "未知周期",
    }
}

fn advance_next(
    next: NaiveDateTime,
    cycle: &str,
    now: NaiveDateTime,
) -> HandlerResult<NaiveDateTime> {
    let base = next.max(now);
    let next = match cycle {
        "day" => base.checked_add_signed(Duration::days(1)),
        "week" => base.checked_add_signed(Duration::weeks(1)),
        "month" => base.checked_add_months(Months::new(1)),
        _ => return Err(bad_request("自动转入周期无效")),
    };
    next.ok_or_else(|| bad_request("下一次自动转入时间超出范围"))
}

async fn account_options(
    state: &AppState,
    dek: &crypto::Dek,
    exclude: &[i64],
) -> HandlerResult<Vec<AccountOption>> {
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut names = super::bills::account_display_names(dek, &accounts, &details);
    let eligible: Vec<AccountOption> = accounts
        .into_iter()
        .filter(|account| is_eligible(account) && !exclude.contains(&account.id))
        .map(|account| AccountOption {
            id: account.id,
            name: names.remove(&account.id).unwrap_or_default(),
            currency: account.currency,
        })
        .collect();
    Ok(eligible)
}

async fn load_project(state: &AppState, id: i64) -> HandlerResult<savings_project::Model> {
    savings_project::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "攒钱项目不存在".into()))
}

async fn validate_account(
    state: &AppState,
    account_id: i64,
    label: &str,
) -> HandlerResult<account::Model> {
    let account = account::Entity::find_by_id(account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request(&format!("{label}不存在")))?;
    if !is_eligible(&account) {
        return Err(bad_request(&format!(
            "{label}必须是支付、银行、储值卡或其他普通账户"
        )));
    }
    Ok(account)
}

async fn parse_auto(
    state: &AppState,
    form: &SavingsFormData,
    savings_account: &account::Model,
) -> HandlerResult<(Option<i64>, i64, String, Option<NaiveDateTime>, bool)> {
    if !form.auto_enabled {
        return Ok((None, 0, "month".into(), None, false));
    }
    if form.source_account_id <= 0 {
        return Err(bad_request("启用自动转入时必须选择资金来源账户"));
    }
    let source = validate_account(state, form.source_account_id, "资金来源账户").await?;
    if source.id == savings_account.id {
        return Err(bad_request("资金来源账户不能是攒钱账户本身"));
    }
    if source.currency != savings_account.currency {
        return Err(bad_request("资金来源账户与攒钱账户必须使用相同货币"));
    }
    if is_savings_account_on(&state.db, source.id).await? {
        return Err(bad_request("资金来源账户不能是另一个攒钱账户"));
    }
    let amount = parse_amount(&form.amount, "自动转入金额")?;
    if !valid_cycle(&form.cycle) {
        return Err(bad_request("自动转入周期无效"));
    }
    let next_run_at = NaiveDateTime::parse_from_str(form.next_run_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("下一次自动转入时间格式不正确"))?;
    Ok((
        Some(source.id),
        amount,
        form.cycle.clone(),
        Some(next_run_at),
        true,
    ))
}

async fn execute_transfer(
    state: &AppState,
    dek: &crypto::Dek,
    from: &account::Model,
    to: &account::Model,
    amount: i64,
    note: &str,
    allow_savings_source: bool,
    project_next: Option<(i64, NaiveDateTime)>,
) -> HandlerResult<()> {
    let delta = amount
        .checked_neg()
        .ok_or_else(|| bad_request("金额超出范围"))?;
    if allow_savings_source {
        super::accounts::ensure_balance_delta_allow_savings(state, dek, from.id, delta).await?;
    } else {
        super::accounts::ensure_balance_delta(state, dek, from.id, delta).await?;
    }
    super::accounts::ensure_balance_delta(state, dek, to.id, amount).await?;
    let transaction = state.db.begin().await.map_err(err500)?;
    transfer::ActiveModel {
        from_account_id: Set(from.id),
        to_account_id: Set(to.id),
        amount: Set(crypto::encrypt_cents(dek, amount)),
        to_amount: Set(crypto::encrypt_cents(dek, amount)),
        fee_rate_bps: Set(crypto::encrypt_cents(dek, 0)),
        fee_amount: Set(crypto::encrypt_cents(dek, 0)),
        note: Set(crypto::encrypt(dek, note.as_bytes())),
        happened_at: Set(chrono::Utc::now().naive_utc()),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    if let Some((project_id, next_run_at)) = project_next {
        let project = savings_project::Entity::find_by_id(project_id)
            .one(&transaction)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("攒钱项目不存在"))?;
        let mut active = project.into_active_model();
        active.next_run_at = Set(Some(next_run_at));
        active.update(&transaction).await.map_err(err500)?;
    }
    transaction.commit().await.map_err(err500)?;
    Ok(())
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let projects = savings_project::Entity::find()
        .order_by_asc(savings_project::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let savings_ids: Vec<i64> = projects.iter().map(|p| p.account_id).collect();
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
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
    let names = super::bills::account_display_names(&dek, &accounts, &details);
    let mut rows = Vec::new();
    for project in &projects {
        let account = accounts
            .iter()
            .find(|account| account.id == project.account_id)
            .ok_or_else(|| err500("攒钱附属账户不存在"))?;
        let balance = super::accounts::current_balance(&state, &dek, account.id).await?;
        let amount = crypto::decrypt_cents(&dek, &project.amount);
        let source_name = project
            .source_account_id
            .and_then(|id| names.get(&id).cloned())
            .unwrap_or_default();
        rows.push(ProjectRow {
            id: project.id,
            name: crypto::decrypt_string(&dek, &project.name),
            account_name: names.get(&account.id).cloned().unwrap_or_default(),
            currency: account.currency.clone(),
            balance: currency::format(balance, &account.currency),
            auto_label: if project.auto_enabled {
                format!(
                    "{} · {} → {}",
                    cycle_label(&project.cycle),
                    source_name,
                    currency::format(amount, &account.currency),
                )
            } else {
                "未启用自动转入".into()
            },
            next_run_value: project
                .next_run_at
                .map(|at| at.format(TIME_FMT).to_string())
                .unwrap_or_default(),
            note: crypto::decrypt_string(&dek, &project.note),
        });
    }
    let accounts = account_options(&state, &dek, &savings_ids).await?;
    let html = SavingsTemplate {
        has_projects: !rows.is_empty(),
        projects: rows,
        funding_accounts: accounts,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let linked: Vec<i64> = savings_project::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|project| project.account_id)
        .collect();
    let funding_accounts = account_options(&state, &dek, &linked).await?;
    let html = SavingsFormTemplate {
        heading: "新建攒钱项目".into(),
        action: "/savings".into(),
        name: String::new(),
        account_name: String::new(),
        account_id: 0,
        note: String::new(),
        auto_enabled: false,
        source_account_id: 0,
        amount: String::new(),
        cycle: "month".into(),
        next_run_at: (chrono::Utc::now() + Duration::days(1))
            .format(TIME_FMT)
            .to_string(),
        savings_accounts: funding_accounts.clone(),
        funding_accounts,
        edit: false,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<SavingsFormData>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let name = form.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad_request("项目名必须为 1 到 100 个字符"));
    }
    let savings_account = validate_account(&state, form.account_id, "攒钱账户").await?;
    if is_savings_account_on(&state.db, savings_account.id).await? {
        return Err(bad_request("该账户已经是攒钱账户"));
    }
    let (source, amount_cents, cycle, next_run_at, auto_enabled) =
        parse_auto(&state, &form, &savings_account).await?;
    if form.note.chars().count() > 1000 {
        return Err(bad_request("说明不能超过 1000 个字符"));
    }
    savings_project::ActiveModel {
        name: Set(crypto::encrypt(&dek, name.as_bytes())),
        account_id: Set(savings_account.id),
        source_account_id: Set(source),
        amount: Set(if auto_enabled {
            crypto::encrypt_cents(&dek, amount_cents)
        } else {
            String::new()
        }),
        cycle: Set(cycle),
        next_run_at: Set(next_run_at),
        auto_enabled: Set(auto_enabled),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/savings"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let project = load_project(&state, id).await?;
    let account = account::Entity::find_by_id(project.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("攒钱附属账户不存在"))?;
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
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
    let mut names = super::bills::account_display_names(&dek, &accounts, &details);
    let linked: Vec<i64> = savings_project::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|project| project.account_id)
        .collect();
    let funding_accounts = accounts
        .into_iter()
        .filter(|item| is_eligible(item) && item.id != account.id)
        .filter(|item| item.currency == account.currency)
        .filter(|item| !linked.contains(&item.id))
        .map(|item| AccountOption {
            id: item.id,
            name: names.remove(&item.id).unwrap_or_default(),
            currency: item.currency,
        })
        .collect();
    let html = SavingsFormTemplate {
        heading: "编辑攒钱项目".into(),
        action: format!("/savings/{id}/edit"),
        name: crypto::decrypt_string(&dek, &project.name),
        account_name: names.remove(&account.id).unwrap_or_default(),
        account_id: account.id,
        note: crypto::decrypt_string(&dek, &project.note),
        auto_enabled: project.auto_enabled,
        source_account_id: project.source_account_id.unwrap_or_default(),
        amount: super::fmt_cents(crypto::decrypt_cents(&dek, &project.amount)),
        cycle: project.cycle,
        next_run_at: project
            .next_run_at
            .map(|at| at.format(TIME_FMT).to_string())
            .unwrap_or_default(),
        savings_accounts: Vec::new(),
        funding_accounts,
        edit: true,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<SavingsFormData>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    let savings_account = account::Entity::find_by_id(project.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("攒钱附属账户不存在"))?;
    let name = form.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad_request("项目名必须为 1 到 100 个字符"));
    }
    let (source, amount_cents, cycle, next_run_at, auto_enabled) =
        parse_auto(&state, &form, &savings_account).await?;
    if form.note.chars().count() > 1000 {
        return Err(bad_request("说明不能超过 1000 个字符"));
    }
    let mut active = project.into_active_model();
    active.name = Set(crypto::encrypt(&dek, name.as_bytes()));
    active.source_account_id = Set(source);
    active.amount = Set(if auto_enabled {
        crypto::encrypt_cents(&dek, amount_cents)
    } else {
        String::new()
    });
    active.cycle = Set(cycle);
    active.next_run_at = Set(next_run_at);
    active.auto_enabled = Set(auto_enabled);
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/savings"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    let balance = super::accounts::current_balance(&state, &dek, project.account_id).await?;
    if balance != 0 {
        return Err(bad_request(
            "攒钱账户仍有余额，请先通过特殊转出清空后再删除项目",
        ));
    }
    savings_project::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/savings"))
}

pub async fn deposit(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DepositForm>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    let savings = account::Entity::find_by_id(project.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("攒钱附属账户不存在"))?;
    let source = validate_account(&state, form.from_account_id, "资金来源账户").await?;
    if source.id == savings.id {
        return Err(bad_request("资金来源账户不能是攒钱账户本身"));
    }
    if source.currency != savings.currency {
        return Err(bad_request("资金来源账户与攒钱账户必须使用相同货币"));
    }
    if is_savings_account_on(&state.db, source.id).await? {
        return Err(bad_request("资金来源账户不能是另一个攒钱账户"));
    }
    let amount = parse_amount(&form.amount, "存入金额")?;
    let name = crypto::decrypt_string(&dek, &project.name);
    execute_transfer(
        &state,
        &dek,
        &source,
        &savings,
        amount,
        &format!("攒钱转入 · {name}"),
        false,
        None,
    )
    .await?;
    Ok(Redirect::to("/savings"))
}

pub async fn withdraw(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<WithdrawForm>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    let savings = account::Entity::find_by_id(project.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("攒钱附属账户不存在"))?;
    if !form.confirm_withdrawal {
        return Err(bad_request("特殊转出必须明确确认"));
    }
    let reason = form.reason.trim();
    if reason.chars().count() < 2 || reason.chars().count() > 200 {
        return Err(bad_request("请填写 2 到 200 个字符的特殊转出原因"));
    }
    let destination = validate_account(&state, form.to_account_id, "转入账户").await?;
    if destination.id == savings.id {
        return Err(bad_request("转入账户不能是攒钱账户本身"));
    }
    if destination.currency != savings.currency {
        return Err(bad_request("转入账户与攒钱账户必须使用相同货币"));
    }
    if is_savings_account_on(&state.db, destination.id).await? {
        return Err(bad_request("特殊转出只能转到普通账户"));
    }
    let amount = parse_amount(&form.amount, "转出金额")?;
    let name = crypto::decrypt_string(&dek, &project.name);
    execute_transfer(
        &state,
        &dek,
        &savings,
        &destination,
        amount,
        &format!("攒钱特殊转出 · {name} · {reason}"),
        true,
        None,
    )
    .await?;
    Ok(Redirect::to("/savings"))
}

pub async fn run_due(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Json<RunDueResponse>> {
    let _guard = state.balance_writes.lock().await;
    let now = chrono::Utc::now().naive_utc();
    let projects = savings_project::Entity::find()
        .filter(savings_project::Column::AutoEnabled.eq(true))
        .filter(savings_project::Column::NextRunAt.is_not_null())
        .filter(savings_project::Column::NextRunAt.lte(now))
        .order_by_asc(savings_project::Column::NextRunAt)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut executed = 0usize;
    let mut failures = Vec::new();
    for project in projects {
        let name = crypto::decrypt_string(&dek, &project.name);
        let Some(source_id) = project.source_account_id else {
            failures.push(format!("{name}：未配置资金来源"));
            continue;
        };
        let source = account::Entity::find_by_id(source_id)
            .one(&state.db)
            .await
            .map_err(err500)?;
        let savings = account::Entity::find_by_id(project.account_id)
            .one(&state.db)
            .await
            .map_err(err500)?;
        let (Some(source), Some(savings)) = (source, savings) else {
            failures.push(format!("{name}：账户不存在"));
            continue;
        };
        if !is_eligible(&source) {
            failures.push(format!("{name}：资金来源账户不是普通账户"));
            continue;
        }
        if source.id == savings.id || source.currency != savings.currency {
            failures.push(format!("{name}：资金来源与攒钱账户不匹配"));
            continue;
        }
        let amount = crypto::decrypt_cents(&dek, &project.amount);
        if amount <= 0 {
            failures.push(format!("{name}：自动转入金额无效"));
            continue;
        }
        let next = match advance_next(project.next_run_at.unwrap_or(now), &project.cycle, now) {
            Ok(next) => next,
            Err((_, error)) => {
                failures.push(format!("{name}：{error}"));
                continue;
            }
        };
        match execute_transfer(
            &state,
            &dek,
            &source,
            &savings,
            amount,
            &format!("攒钱自动转入 · {name}"),
            false,
            Some((project.id, next)),
        )
        .await
        {
            Ok(()) => executed += 1,
            Err((_, error)) => failures.push(format!("{name}：{error}")),
        }
    }
    let message = if executed == 0 && failures.is_empty() {
        "没有待执行的自动攒钱".into()
    } else if failures.is_empty() {
        format!("已完成 {executed} 笔自动攒钱")
    } else {
        format!("完成 {executed} 笔，{} 笔未执行", failures.len())
    };
    Ok(Json(RunDueResponse {
        ok: failures.is_empty(),
        executed,
        failures,
        message,
    }))
}

#[cfg(test)]
mod tests {
    use super::advance_next;
    use chrono::NaiveDate;

    fn at(year: i32, month: u32, day: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap()
    }

    #[test]
    fn advances_daily_weekly_and_monthly_from_later_base() {
        let now = at(2026, 1, 10);
        assert_eq!(
            advance_next(at(2026, 1, 1), "day", now).unwrap(),
            at(2026, 1, 11)
        );
        assert_eq!(
            advance_next(at(2026, 1, 1), "week", now).unwrap(),
            at(2026, 1, 17)
        );
        assert_eq!(
            advance_next(at(2026, 1, 15), "month", now).unwrap(),
            at(2026, 2, 15)
        );
        assert!(advance_next(now, "year", now).is_err());
    }
}
