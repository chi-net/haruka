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
    entity::{account, account_detail, meta, savings_project},
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
    kind: String,
    kind_label: String,
    account_name: String,
    balance: String,
    target: String,
    progress: String,
    auto_label: String,
    next_run_value: String,
    status_label: String,
    realized: bool,
    redeemed_item: bool,
    realized_amount: String,
    realized_at: String,
    end_at: String,
    realized_note: String,
    can_redeem: bool,
    note: String,
}

#[derive(Template)]
#[template(path = "savings.html")]
struct SavingsTemplate {
    projects: Vec<ProjectRow>,
    has_projects: bool,
    legacy_targets: bool,
}

#[derive(Template)]
#[template(path = "savings_targets.html")]
struct SavingsTargetsTemplate {
    rows: Vec<TargetRow>,
}

struct TargetRow {
    id: i64,
    name: String,
    target: String,
}

#[derive(Template)]
#[template(path = "savings_form.html")]
struct SavingsFormTemplate {
    heading: String,
    action: String,
    name: String,
    kind: String,
    target_amount: String,
    account_name: String,
    account_id: i64,
    note: String,
    auto_enabled: bool,
    amount: String,
    cycle: String,
    next_run_at: String,
    end_at: String,
    savings_accounts: Vec<AccountOption>,
    edit: bool,
}

#[derive(Deserialize)]
pub struct SavingsFormData {
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    target_amount: String,
    account_id: i64,
    #[serde(default)]
    note: String,
    #[serde(default)]
    auto_enabled: bool,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    cycle: String,
    #[serde(default)]
    next_run_at: String,
    #[serde(default)]
    end_at: String,
}

#[derive(Deserialize)]
pub struct DepositForm {
    amount: String,
}

#[derive(Deserialize)]
pub struct QuickDepositForm {
    project_id: i64,
    amount: String,
}

#[derive(Deserialize)]
pub struct WithdrawForm {
    amount: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    confirm_withdrawal: bool,
}

#[derive(Deserialize)]
pub struct RedeemForm {
    #[serde(default)]
    note: String,
    #[serde(default)]
    confirm_redeem: bool,
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

/// 实体物品兑现后附属账户自动解锁；资金目标达成后仍保持只进不出。
pub(crate) fn saved_amount(project: &savings_project::Model, dek: &crypto::Dek) -> i64 {
    crypto::decrypt_cents(dek, &project.saved_amount).max(0)
}

pub(crate) async fn saved_amount_on<C: ConnectionTrait>(
    db: &C,
    dek: &crypto::Dek,
    account_id: i64,
) -> HandlerResult<i64> {
    let projects = savings_project::Entity::find()
        .filter(savings_project::Column::AccountId.eq(account_id))
        .all(db)
        .await
        .map_err(err500)?;
    Ok(projects
        .iter()
        .map(|project| saved_amount(project, dek))
        .fold(0i64, |total, amount| total.saturating_add(amount)))
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

async fn has_legacy_targets(state: &AppState, dek: &crypto::Dek) -> HandlerResult<bool> {
    let projects = savings_project::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let _ = dek;
    Ok(projects
        .iter()
        .any(|project| crypto::decrypt_cents(dek, &project.target_amount) <= 0))
}

pub async fn targets_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let projects = savings_project::Entity::find()
        .order_by_asc(savings_project::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let rows = projects
        .into_iter()
        .filter(|project| crypto::decrypt_cents(&dek, &project.target_amount) <= 0)
        .map(|project| TargetRow {
            id: project.id,
            name: crypto::decrypt_string(&dek, &project.name),
            target: String::new(),
        })
        .collect();
    let html = SavingsTargetsTemplate { rows }.render().map_err(err500)?;
    Ok(Html(html))
}

pub async fn apply_targets(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(values): Form<HashMap<String, String>>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let projects = savings_project::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let tx = state.db.begin().await.map_err(err500)?;
    for project in projects {
        if crypto::decrypt_cents(&dek, &project.target_amount) > 0 {
            continue;
        }
        let value = values
            .get(&format!("target_{}", project.id))
            .ok_or_else(|| bad_request("请为所有历史攒钱项目填写目标金额"))?;
        let target = parse_amount(value, "目标金额")?;
        let mut active = project.into_active_model();
        active.target_amount = Set(crypto::encrypt_cents(&dek, target));
        active.update(&tx).await.map_err(err500)?;
    }
    if let Some(row) = meta::Entity::find_by_id(1).one(&tx).await.map_err(err500)? {
        let mut active = row.into_active_model();
        active.savings_target_state = Set("done".into());
        active.update(&tx).await.map_err(err500)?;
    }
    tx.commit().await.map_err(err500)?;
    Ok(Redirect::to("/savings"))
}

fn parse_kind_target(form: &SavingsFormData) -> HandlerResult<(String, i64)> {
    let kind = if form.kind.trim().is_empty() {
        "item"
    } else {
        form.kind.trim()
    };
    match kind {
        "item" | "fund" => {
            let target = parse_amount(&form.target_amount, "目标金额")?;
            Ok((kind.to_string(), target))
        }
        _ => Err(bad_request("攒钱项目类型无效")),
    }
}

fn parse_end_at(value: &str) -> HandlerResult<Option<NaiveDateTime>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let end_at = NaiveDateTime::parse_from_str(value, TIME_FMT)
        .map_err(|_| bad_request("结束时间格式不正确"))?;
    Ok(Some(end_at))
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

async fn reconcile_fund_goal(
    state: &AppState,
    dek: &crypto::Dek,
    project: &mut savings_project::Model,
    balance: i64,
) -> HandlerResult<bool> {
    if project.status != "active" || project.kind != "fund" {
        return Ok(false);
    }
    let target = crypto::decrypt_cents(dek, &project.target_amount);
    if target <= 0 || balance < target {
        return Ok(false);
    }
    let mut active = project.clone().into_active_model();
    active.status = Set("realized".into());
    active.realized_amount = Set(crypto::encrypt_cents(dek, 0));
    active.realized_at = Set(Some(chrono::Utc::now()));
    active.realized_note = Set(crypto::encrypt(dek, "资金目标达成".as_bytes()));
    active.auto_enabled = Set(false);
    active.next_run_at = Set(None);
    *project = active.update(&state.db).await.map_err(err500)?;
    Ok(true)
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
    form: &SavingsFormData,
) -> HandlerResult<(Option<i64>, i64, String, Option<NaiveDateTime>, bool)> {
    if !form.auto_enabled {
        return Ok((None, 0, "month".into(), None, false));
    }
    let amount = parse_amount(&form.amount, "自动划入金额")?;
    if !valid_cycle(&form.cycle) {
        return Err(bad_request("自动划入周期无效"));
    }
    let next_run_at = NaiveDateTime::parse_from_str(form.next_run_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("下一次自动划入时间格式不正确"))?;
    Ok((None, amount, form.cycle.clone(), Some(next_run_at), true))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let mut projects = savings_project::Entity::find()
        .order_by_asc(savings_project::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
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
    for project in &mut projects {
        let account = accounts
            .iter()
            .find(|account| account.id == project.account_id)
            .ok_or_else(|| err500("攒钱附属账户不存在"))?;
        let saved = saved_amount(project, &dek);
        reconcile_fund_goal(&state, &dek, project, saved).await?;
        let amount = crypto::decrypt_cents(&dek, &project.amount);
        let target = crypto::decrypt_cents(&dek, &project.target_amount);
        let realized = project.status == "realized";
        let progress = if target > 0 {
            format!(
                "{}%",
                (i128::from(saved.max(0)) * 100 / i128::from(target)).min(100)
            )
        } else {
            String::new()
        };
        rows.push(ProjectRow {
            id: project.id,
            name: crypto::decrypt_string(&dek, &project.name),
            kind: project.kind.clone(),
            kind_label: if project.kind == "fund" {
                "资金目标".into()
            } else {
                "实体物品".into()
            },
            account_name: names.get(&account.id).cloned().unwrap_or_default(),
            balance: currency::format(saved, &account.currency),
            target: currency::format(target, &account.currency),
            progress,
            auto_label: if project.auto_enabled {
                format!(
                    "{} · 自动划入 {}",
                    cycle_label(&project.cycle),
                    currency::format(amount, &account.currency),
                )
            } else {
                "未启用自动转入".into()
            },
            next_run_value: project
                .next_run_at
                .map(|at| at.format(TIME_FMT).to_string())
                .unwrap_or_default(),
            status_label: if realized {
                if project.kind == "fund" {
                    "目标达成".into()
                } else {
                    "已兑现".into()
                }
            } else {
                "进行中".into()
            },
            realized,
            redeemed_item: realized && project.kind == "item",
            realized_amount: if realized && project.kind == "item" {
                currency::format(
                    crypto::decrypt_cents(&dek, &project.realized_amount),
                    &account.currency,
                )
            } else {
                String::new()
            },
            realized_at: project
                .realized_at
                .map(|at| at.format(TIME_FMT).to_string())
                .unwrap_or_default(),
            end_at: project
                .end_at
                .map(|at| at.format(TIME_FMT).to_string())
                .unwrap_or_default(),
            realized_note: crypto::decrypt_string(&dek, &project.realized_note),
            can_redeem: project.kind == "item" && !realized,
            note: crypto::decrypt_string(&dek, &project.note),
        });
    }
    let legacy_targets = has_legacy_targets(&state, &dek).await?;
    let html = SavingsTemplate {
        has_projects: !rows.is_empty(),
        projects: rows,
        legacy_targets,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let funding_accounts = account_options(&state, &dek, &[]).await?;
    let html = SavingsFormTemplate {
        heading: "新建攒钱项目".into(),
        action: "/savings".into(),
        name: String::new(),
        kind: "item".into(),
        target_amount: String::new(),
        account_name: String::new(),
        account_id: 0,
        note: String::new(),
        auto_enabled: false,
        amount: String::new(),
        cycle: "month".into(),
        next_run_at: (chrono::Utc::now() + Duration::days(1))
            .format(TIME_FMT)
            .to_string(),
        end_at: String::new(),
        savings_accounts: funding_accounts,
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
    let (kind, target_cents) = parse_kind_target(&form)?;
    let end_at = parse_end_at(&form.end_at)?;
    let (_source, amount_cents, cycle, next_run_at, auto_enabled) = parse_auto(&form).await?;
    if form.note.chars().count() > 1000 {
        return Err(bad_request("说明不能超过 1000 个字符"));
    }
    let mut project = savings_project::ActiveModel {
        name: Set(crypto::encrypt(&dek, name.as_bytes())),
        kind: Set(kind),
        target_amount: Set(crypto::encrypt_cents(&dek, target_cents)),
        saved_amount: Set(crypto::encrypt_cents(&dek, 0)),
        end_at: Set(end_at),
        account_id: Set(savings_account.id),
        source_account_id: Set(None),
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
    let saved = saved_amount(&project, &dek);
    reconcile_fund_goal(&state, &dek, &mut project, saved).await?;
    Ok(Redirect::to("/savings"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let project = load_project(&state, id).await?;
    if project.status == "realized" {
        return Err(bad_request("已兑现或已达成目标的项目不可编辑"));
    }
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
    let html = SavingsFormTemplate {
        heading: "编辑攒钱项目".into(),
        action: format!("/savings/{id}/edit"),
        name: crypto::decrypt_string(&dek, &project.name),
        kind: project.kind.clone(),
        target_amount: if crypto::decrypt_cents(&dek, &project.target_amount) > 0 {
            super::fmt_cents(crypto::decrypt_cents(&dek, &project.target_amount))
        } else {
            String::new()
        },
        account_name: names.remove(&account.id).unwrap_or_default(),
        account_id: account.id,
        note: crypto::decrypt_string(&dek, &project.note),
        auto_enabled: project.auto_enabled,
        amount: super::fmt_cents(crypto::decrypt_cents(&dek, &project.amount)),
        cycle: project.cycle,
        next_run_at: project
            .next_run_at
            .map(|at| at.format(TIME_FMT).to_string())
            .unwrap_or_default(),
        end_at: project
            .end_at
            .map(|at| at.format(TIME_FMT).to_string())
            .unwrap_or_default(),
        savings_accounts: Vec::new(),
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
    if project.status == "realized" {
        return Err(bad_request("已兑现或已达成目标的项目不可编辑"));
    }
    let savings_account = account::Entity::find_by_id(project.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("攒钱附属账户不存在"))?;
    let name = form.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad_request("项目名必须为 1 到 100 个字符"));
    }
    let (kind, target_cents) = parse_kind_target(&form)?;
    let end_at = parse_end_at(&form.end_at)?;
    let (_source, amount_cents, cycle, next_run_at, auto_enabled) = parse_auto(&form).await?;
    if form.note.chars().count() > 1000 {
        return Err(bad_request("说明不能超过 1000 个字符"));
    }
    let mut active = project.into_active_model();
    active.name = Set(crypto::encrypt(&dek, name.as_bytes()));
    active.kind = Set(kind);
    active.target_amount = Set(if target_cents > 0 {
        crypto::encrypt_cents(&dek, target_cents)
    } else {
        String::new()
    });
    active.source_account_id = Set(None);
    active.amount = Set(if auto_enabled {
        crypto::encrypt_cents(&dek, amount_cents)
    } else {
        String::new()
    });
    active.cycle = Set(cycle);
    active.next_run_at = Set(next_run_at);
    active.end_at = Set(end_at);
    active.auto_enabled = Set(auto_enabled);
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    let mut project = active.update(&state.db).await.map_err(err500)?;
    let balance = super::accounts::current_balance(&state, &dek, savings_account.id).await?;
    reconcile_fund_goal(&state, &dek, &mut project, balance).await?;
    Ok(Redirect::to("/savings"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    if saved_amount(&project, &dek) != 0 {
        return Err(bad_request(
            "该项目仍有冻结的攒钱金额，请先通过特殊情况转出后再删除项目",
        ));
    }
    savings_project::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/savings"))
}

pub async fn quick_deposit(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<QuickDepositForm>,
) -> HandlerResult<Redirect> {
    allocate_to_project(&state, &dek, form.project_id, &form.amount).await?;
    Ok(Redirect::to("/savings"))
}

pub async fn deposit(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DepositForm>,
) -> HandlerResult<Redirect> {
    allocate_to_project(&state, &dek, id, &form.amount).await?;
    Ok(Redirect::to("/savings"))
}

async fn allocate_to_project(
    state: &AppState,
    dek: &crypto::Dek,
    project_id: i64,
    amount_input: &str,
) -> HandlerResult<()> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(state, project_id).await?;
    if project.status == "realized" {
        return Err(bad_request("项目已兑现或已达成目标，不能再存入"));
    }
    let savings = account::Entity::find_by_id(project.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("攒钱附属账户不存在"))?;
    let amount = parse_amount(amount_input, "存入金额")?;
    let balance = super::accounts::current_balance(state, dek, savings.id).await?;
    let saved = saved_amount(&project, dek);
    let available = balance.saturating_sub(saved).max(0);
    if amount > available {
        return Err(bad_request("存入金额超过附属账户未冻结余额"));
    }
    let new_saved = saved
        .checked_add(amount)
        .ok_or_else(|| bad_request("攒钱金额超出范围"))?;
    let mut active = project.into_active_model();
    active.saved_amount = Set(crypto::encrypt_cents(dek, new_saved));
    active.update(&state.db).await.map_err(err500)?;
    if let Ok(mut refreshed) = load_project(state, project_id).await {
        reconcile_fund_goal(state, dek, &mut refreshed, new_saved).await?;
    }
    Ok(())
}

pub async fn withdraw(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<WithdrawForm>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    if !form.confirm_withdrawal {
        return Err(bad_request("特殊转出必须明确确认"));
    }
    let reason = form.reason.trim();
    if reason.chars().count() < 2 || reason.chars().count() > 200 {
        return Err(bad_request("请填写 2 到 200 个字符的特殊转出原因"));
    }
    let amount = parse_amount(&form.amount, "转出金额")?;
    let saved = saved_amount(&project, &dek);
    if amount > saved {
        return Err(bad_request("转出金额超过当前冻结的攒钱金额"));
    }
    let name = crypto::decrypt_string(&dek, &project.name);
    let old_note = crypto::decrypt_string(&dek, &project.note);
    let note = if old_note.trim().is_empty() {
        format!("特殊情况转出 · {name} · {reason}")
    } else {
        format!("{old_note}\n特殊情况转出 · {name} · {reason}")
    };
    let mut active = project.into_active_model();
    active.saved_amount = Set(crypto::encrypt_cents(&dek, saved - amount));
    active.note = Set(crypto::encrypt(&dek, note.as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/savings"))
}

pub async fn redeem(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<RedeemForm>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let project = load_project(&state, id).await?;
    if project.kind != "item" || project.status != "active" {
        return Err(bad_request("只有进行中的实体物品项目可以兑现"));
    }
    if !form.confirm_redeem {
        return Err(bad_request("兑现前必须明确确认"));
    }
    if form.note.chars().count() > 1000 {
        return Err(bad_request("兑现说明不能超过 1000 个字符"));
    }
    let note = form.note.trim();
    let now = chrono::Utc::now();
    let mut active = project.into_active_model();
    active.status = Set("realized".into());
    active.saved_amount = Set(crypto::encrypt_cents(&dek, 0));
    active.realized_amount = Set(String::new());
    active.realized_at = Set(Some(now));
    active.realized_note = Set(crypto::encrypt(&dek, note.as_bytes()));
    active.auto_enabled = Set(false);
    active.next_run_at = Set(None);
    active.update(&state.db).await.map_err(err500)?;
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
        let savings = account::Entity::find_by_id(project.account_id)
            .one(&state.db)
            .await
            .map_err(err500)?;
        let Some(savings) = savings else {
            failures.push(format!("{name}：攒钱账户不存在"));
            continue;
        };
        let mut amount = crypto::decrypt_cents(&dek, &project.amount);
        if amount <= 0 {
            failures.push(format!("{name}：自动划入金额无效"));
            continue;
        }
        let saved = saved_amount(&project, &dek);
        let balance = super::accounts::current_balance(&state, &dek, savings.id).await?;
        if project.kind == "fund" {
            let target = crypto::decrypt_cents(&dek, &project.target_amount);
            if saved >= target {
                let mut refreshed = project.clone();
                if let Err((_, error)) =
                    reconcile_fund_goal(&state, &dek, &mut refreshed, saved).await
                {
                    failures.push(format!("{name}：{error}"));
                }
                continue;
            }
            amount = amount.min(target - saved);
        }
        let available = balance.saturating_sub(saved).max(0);
        amount = amount.min(available);
        if amount <= 0 {
            failures.push(format!("{name}：附属账户未冻结余额不足"));
            continue;
        }
        let next = match advance_next(project.next_run_at.unwrap_or(now), &project.cycle, now) {
            Ok(next) => next,
            Err((_, error)) => {
                failures.push(format!("{name}：{error}"));
                continue;
            }
        };
        let mut active = project.clone().into_active_model();
        active.saved_amount = Set(crypto::encrypt_cents(&dek, saved + amount));
        active.next_run_at = Set(Some(next));
        active.update(&state.db).await.map_err(err500)?;
        executed += 1;
        if let Ok(mut refreshed) = load_project(&state, project.id).await {
            reconcile_fund_goal(&state, &dek, &mut refreshed, saved + amount).await?;
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
