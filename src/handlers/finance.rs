use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::{
    extract::{Extension, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Datelike, Utc};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    crypto, currency,
    entity::{account, bill, financial_plan, recurring_investment},
    financial_planning::{self as planning, Config, LinkedPlan, Saved},
    investment_funds, AppState, SessionDek,
};

use super::{investments, ClientTimeZone};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn bad(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}
fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
fn conflict() -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        "理财配置或预览已变化，请重新保存并查看方案后再确认".into(),
    )
}

#[derive(Template)]
#[template(path = "finance.html")]
struct FinanceTemplate {
    data_json: String,
}

#[derive(Deserialize)]
pub struct SaveForm {
    config_json: String,
    revision: i64,
}
#[derive(Deserialize)]
pub struct GenerateForm {
    revision: i64,
    generation_token: String,
}

#[derive(Serialize)]
struct AccountOption {
    id: i64,
    name: String,
    kind: String,
    currency: String,
}
#[derive(Serialize)]
struct IndexOption {
    code: &'static str,
    name: &'static str,
}
#[derive(Serialize)]
struct IncomeMonth {
    label: String,
    total: String,
    count: usize,
}
#[derive(Serialize)]
struct IncomeView {
    months: Vec<IncomeMonth>,
    average: String,
    average_input: String,
    has_estimate: bool,
    time_zone: String,
    explanation: String,
}
#[derive(Serialize)]
struct PreviewRow {
    key: String,
    name: String,
    share: String,
    monthly_target: String,
    daily_amount: String,
    estimated_monthly: String,
    fee_per_day: String,
    strategy_label: String,
    fund_label: String,
    current_value: String,
    target_value: String,
    rebalance_gap: String,
    plan_id: Option<i64>,
}
#[derive(Serialize)]
struct Preview {
    income: String,
    income_source: String,
    monthly_investment: String,
    monthly_remaining: String,
    emergency_target: String,
    reserve_current: String,
    reserve_gap: String,
    reserve_months: String,
    portfolio_total: String,
    rows: Vec<PreviewRow>,
    can_generate: bool,
    generation_error: String,
    equity_percent: String,
    currency: String,
    from_currency: String,
    exchange_info: String,
}
#[derive(Serialize)]
struct FinanceData {
    config: Config,
    revision: i64,
    accounts: Vec<AccountOption>,
    index_options: Vec<IndexOption>,
    moving_average_options: &'static [i32],
    income: IncomeView,
    preview: Option<Preview>,
    warnings: Vec<String>,
    linked_plans: Vec<LinkedPlan>,
    generation_token: String,
    notice: String,
    today: String,
}

// The amounts in this proposal are the only amounts generation is allowed to use.
struct Proposal {
    data: FinanceData,
    daily_amounts: Vec<i64>,
    plans: HashMap<i64, recurring_investment::Model>,
}

async fn load_saved(state: &AppState, dek: &crypto::Dek) -> HandlerResult<(Saved, i64)> {
    if let Some(model) = financial_plan::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        let saved: Saved =
            serde_json::from_str(&crypto::decrypt_string(dek, &model.payload)).map_err(err500)?;
        planning::validate_config(&saved.config).map_err(err500)?;
        return Ok((saved, model.revision));
    }
    let code = currency::default_currency(state).await.map_err(err500)?;
    let start = investments::china_today()
        .succ_opt()
        .ok_or_else(|| err500("开始日期超出范围"))?;
    Ok((
        Saved {
            config: planning::default_config(code, start),
            linked_plans: Vec::new(),
        },
        0,
    ))
}

fn normalize(mut config: Config) -> HandlerResult<Config> {
    config.name = config.name.trim().to_owned();
    config.manual_monthly_income = config.manual_monthly_income.trim().to_owned();
    for row in &mut config.allocations {
        row.name = row.name.trim().to_owned();
        row.index_code = row.index_code.trim().to_owned();
        if row.strategy == "fixed" {
            row.index_code.clear();
            row.moving_average_days = 180;
        }
    }
    planning::validate_config(&config).map_err(bad)?;
    Ok(config)
}

async fn validate_accounts(
    state: &AppState,
    config: &Config,
    by_id: &HashMap<i64, &account::Model>,
) -> HandlerResult<()> {
    let source = config
        .from_account_id
        .map(|id| by_id.get(&id).copied().ok_or_else(|| bad("扣款账户不存在")))
        .transpose()?;
    if let Some(source) = source {
        investment_funds::validate_money_account(state, source).await?;
        if matches!(source.kind.as_str(), "credit_card" | "credit_service") {
            return Err(bad("信用卡和信贷服务不能作为定投扣款账户"));
        }
    }
    if let Some(id) = config.parent_account_id {
        let group = by_id.get(&id).ok_or_else(|| bad("基金分组不存在"))?;
        if group.kind != "investment" || group.parent_id.is_some() {
            return Err(bad("请选择有效的投资基金分组"));
        }
        if source.is_some_and(|from| from.currency != group.currency) {
            return Err(bad("新基金分组必须与扣款账户同币种"));
        }
    }
    for row in &config.allocations {
        if let Some(id) = row.fund_account_id {
            let fund = by_id
                .get(&id)
                .ok_or_else(|| bad(format!("{}绑定的基金不存在，请重新选择", row.name)))?;
            investment_funds::validate_money_account(state, fund).await?;
            if fund.kind != "investment_fund" {
                return Err(bad("资产配置只能绑定具体基金，不能绑定投资分组"));
            }
            if let Some(source) = source {
                investments::validate_plan_accounts(state, source, fund, &row.strategy).await?;
            }
        }
    }
    for id in &config.reserve_account_ids {
        let item = by_id
            .get(id)
            .ok_or_else(|| bad("应急金账户不存在，请重新选择"))?;
        investment_funds::validate_money_account(state, item).await?;
        if matches!(item.kind.as_str(), "credit_card" | "credit_service") {
            return Err(bad("授信额度不是应急金，请选择非信用账户"));
        }
    }
    Ok(())
}

async fn infer_income(
    state: &AppState,
    dek: &crypto::Dek,
    zone: ClientTimeZone,
    config: &Config,
    by_id: &HashMap<i64, &account::Model>,
) -> HandlerResult<(IncomeView, Option<i64>)> {
    let windows = planning::complete_months(zone.today(), config.income_months).map_err(err500)?;
    let records: Vec<_> = bill::Entity::find()
        .filter(bill::Column::Kind.eq("income"))
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|item| {
            let date = zone.date(item.happened_at);
            date >= windows[0].0 && date < windows[windows.len() - 1].1
        })
        .collect();
    let mut currencies = HashSet::new();
    let mut counts = vec![0usize; windows.len()];
    for item in &records {
        let account = by_id
            .get(&item.account_id)
            .ok_or_else(|| err500("收入关联账户不存在"))?;
        currencies.insert(account.currency.clone());
        let date = zone.date(item.happened_at);
        if let Some(index) = windows
            .iter()
            .position(|(start, end)| date >= *start && date < *end)
        {
            counts[index] += 1;
        }
    }
    let rates = currency::RateTable::load(state, currencies, &config.currency, zone.today()).await;
    let mut totals = vec![0i64; windows.len()];
    let mut rate_failure = None;
    if let Ok(rates) = rates {
        for item in &records {
            let date = zone.date(item.happened_at);
            let index = windows
                .iter()
                .position(|(start, end)| date >= *start && date < *end)
                .ok_or_else(|| err500("收入月份超出推算区间"))?;
            let native = crypto::decrypt_cents(dek, &item.amount);
            if native < 0 {
                return Err(err500("收入记录金额不能为负"));
            }
            let account = by_id
                .get(&item.account_id)
                .ok_or_else(|| err500("收入关联账户不存在"))?;
            let amount = rates.convert(native, &account.currency).map_err(err500)?;
            totals[index] = totals[index]
                .checked_add(amount)
                .ok_or_else(|| err500("月收入合计超出范围"))?;
        }
    } else if let Err(message) = rates {
        rate_failure = Some(message);
    }
    let average = if rate_failure.is_some() {
        None
    } else {
        planning::average_income(&totals).map_err(err500)?
    };
    let explanation = match rate_failure.as_ref() {
        Some(message) => format!("收入无法按方案币种折算：{message}。不会按 1:1 换算，请手动填写月生活费。"),
        None => format!("按 {} 时区的最近 {} 个完整自然月计算，包含零收入月，排除本月；只统计普通收入，不计转账与借还。异币种按当前最近可用参考汇率折算。收入含一次性进账，不等于可投资结余，请核对或手动覆盖。", zone.0, config.income_months),
    };
    let months = windows
        .iter()
        .enumerate()
        .map(|(index, (start, _))| IncomeMonth {
            label: start.format("%Y-%m").to_string(),
            total: if rate_failure.is_some() {
                "未能折算".into()
            } else {
                currency::format(totals[index], &config.currency)
            },
            count: counts[index],
        })
        .collect();
    Ok((
        IncomeView {
            months,
            average: average
                .map(|amount| currency::format(amount, &config.currency))
                .unwrap_or_else(|| "无法推算，请手动填写".into()),
            average_input: average.map(super::fmt_cents).unwrap_or_default(),
            has_estimate: average.is_some(),
            time_zone: zone.0.name().into(),
            explanation,
        },
        average,
    ))
}

fn percent(bps: i64) -> String {
    format!("{}%", Decimal::new(bps, 2).normalize())
}
fn product(amount: i64, count: u32) -> HandlerResult<i64> {
    amount
        .checked_mul(i64::from(count))
        .ok_or_else(|| bad("理财规划金额超出范围"))
}

async fn proposal(
    state: &AppState,
    dek: &crypto::Dek,
    zone: ClientTimeZone,
    saved: Saved,
    revision: i64,
    notice: String,
    allow_invalid_accounts: bool,
) -> HandlerResult<Proposal> {
    let config = &saved.config;
    planning::validate_config(config).map_err(bad)?;
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let by_id: HashMap<_, _> = accounts.iter().map(|item| (item.id, item)).collect();
    let account_error = match validate_accounts(state, config, &by_id).await {
        Ok(()) => None,
        Err((status, message)) if allow_invalid_accounts && status == StatusCode::BAD_REQUEST => {
            Some(message)
        }
        Err(error) => return Err(error),
    };
    let names = investment_funds::display_names(dek, &accounts);
    let (income, inferred) = infer_income(state, dek, zone, config, &by_id).await?;
    let basis = if config.manual_monthly_income.is_empty() {
        inferred
    } else {
        Some(planning::parse_money(&config.manual_monthly_income).map_err(bad)?)
    };
    let plan_ids: Vec<_> = saved.linked_plans.iter().map(|link| link.plan_id).collect();
    let plans: HashMap<_, _> = if plan_ids.is_empty() {
        HashMap::new()
    } else {
        recurring_investment::Entity::find()
            .filter(recurring_investment::Column::Id.is_in(plan_ids))
            .all(&state.db)
            .await
            .map_err(err500)?
            .into_iter()
            .map(|item| (item.id, item))
            .collect()
    };
    let mut warnings = vec!["这里只设计账本中的资金分配，不连接银行或购买金融产品；参考模板不是个性化投资建议，也不保证收益。".into(), "每日基准向下取整到分；实际月扣款随中国交易日数变化。聪明定投为基准的 50%–150%，手续费另计，月预算不是自动扣款硬上限。".into()];
    if investments::china_today().year() > 2026 {
        warnings.push("内置中国交易日休市数据只覆盖 2025/2026，请在定投页补充后续休市日。".into());
    }
    let equity = config
        .allocations
        .iter()
        .filter(|row| row.asset_class == "equity")
        .map(|row| row.share_bps)
        .sum::<i64>();
    if equity > 5_000 {
        warnings.push(format!("当前权益类占定投资金 {}。文章中成长期的 50% 参考不是硬限制；这不是全部资产的权益比例，请按自己的波动承受能力调整。", percent(equity)));
    }
    let mut daily_amounts = Vec::new();
    let mut token = Sha256::new();
    token.update(serde_json::to_vec(&saved).map_err(err500)?);
    token.update(revision.to_be_bytes());
    let preview = if let Some(basis) = basis.filter(|_| account_error.is_none()) {
        let budget = planning::monthly_budget(basis, config.investment_bps).map_err(bad)?;
        let shares: Vec<_> = config.allocations.iter().map(|row| row.share_bps).collect();
        let targets = planning::allocate_monthly(budget, &shares).map_err(bad)?;
        let source = config
            .from_account_id
            .and_then(|id| by_id.get(&id).copied());
        let from_currency = source
            .map(|item| item.currency.as_str())
            .unwrap_or(&config.currency);
        let mut exchange_info = "方案与扣款账户同币种，无需汇率换算".to_string();
        let converted_budget = if from_currency == config.currency {
            budget
        } else {
            let info =
                currency::rate_with_info(state, &config.currency, from_currency, zone.today())
                    .await
                    .map_err(err500)?;
            exchange_info = format!(
                "1 {} = {} {} · 参考日期 {} · 缓存抓取 {}",
                config.currency,
                info.rate,
                from_currency,
                info.rate_date,
                info.fetched_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_else(|| "无".into())
            );
            (Decimal::from(budget) * info.rate)
                .round()
                .to_i64()
                .ok_or_else(|| bad("定投换算金额超出范围"))?
        };
        let source_targets = planning::allocate_monthly(converted_budget, &shares).map_err(bad)?;
        token.update(basis.to_be_bytes());
        token.update(converted_budget.to_be_bytes());
        let required_ids: HashSet<_> = config
            .reserve_account_ids
            .iter()
            .copied()
            .chain(
                config
                    .allocations
                    .iter()
                    .filter_map(|row| row.fund_account_id),
            )
            .chain(config.from_account_id)
            .collect();
        let mut balances = HashMap::new();
        for id in &required_ids {
            balances.insert(
                *id,
                super::accounts::current_balance(state, dek, *id).await?,
            );
        }
        let currencies: HashSet<_> = required_ids
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|item| item.currency.clone())
            .collect();
        let rates = currency::RateTable::load(state, currencies, &config.currency, zone.today())
            .await
            .map_err(err500)?;
        let mut reserve = 0i64;
        for id in &config.reserve_account_ids {
            let item = by_id.get(id).ok_or_else(|| bad("应急账户已删除"))?;
            let value = rates
                .convert(balances[id], &item.currency)
                .map_err(err500)?;
            reserve = reserve
                .checked_add(value)
                .ok_or_else(|| bad("应急金合计超出范围"))?;
        }
        let mut values = Vec::with_capacity(config.allocations.len());
        let mut portfolio = 0i64;
        for row in &config.allocations {
            let value = if let Some(id) = row.fund_account_id {
                rates
                    .convert(balances[&id], &by_id[&id].currency)
                    .map_err(err500)?
            } else {
                0
            };
            if value < 0 {
                return Err(err500("基金持仓不能为负"));
            }
            portfolio = portfolio
                .checked_add(value)
                .ok_or_else(|| bad("持仓合计超出范围"))?;
            values.push(value);
        }
        let holding_targets = planning::allocate_monthly(portfolio, &shares).map_err(bad)?;
        let goal = product(basis, config.emergency_months)?;
        let gap = goal.saturating_sub(reserve).max(0);
        if config.emergency_months > 0 && reserve < goal {
            warnings.push(
                "应急金尚未达到当前目标，建议先留足可随时动用的资金；这里不会强制禁止投资。".into(),
            );
        }
        if config.reserve_account_ids.iter().any(|id| {
            config
                .allocations
                .iter()
                .any(|row| row.fund_account_id == Some(*id))
        }) {
            warnings.push("应急金账户与投资配置存在重叠；同一笔钱不能同时承担应急支出和长期持仓用途，请自行核对。".into());
        }
        let mut generation_error = if revision == 0 {
            "请先保存配置并查看预览".into()
        } else if source.is_none() && budget > 0 {
            "请选择非信用扣款账户并保存".into()
        } else {
            String::new()
        };
        let mut rows = Vec::with_capacity(config.allocations.len());
        let mut daily_debit = 0i64;
        for (index, row) in config.allocations.iter().enumerate() {
            let daily = planning::per_trade_amount(source_targets[index], config.trading_days)
                .map_err(bad)?;
            daily_amounts.push(daily);
            let fee = investments::calculate_fee(daily, row.fee_rate_bps)?;
            daily_debit = daily_debit
                .checked_add(daily)
                .and_then(|sum| sum.checked_add(fee))
                .ok_or_else(|| bad("每日定投合计超出范围"))?;
            if budget > 0 && row.share_bps > 0 && daily == 0 {
                generation_error = format!(
                    "{}的每日金额不足 1 分，请增加生活费/投资比例或减少估算交易日",
                    row.name
                );
            }
            let link = saved.linked_plans.iter().find(|link| link.key == row.key);
            let existing = link.and_then(|link| plans.get(&link.plan_id));
            if let Some(plan) = existing {
                token.update(
                    serde_json::to_vec(&(
                        plan.id,
                        plan.from_account_id,
                        plan.fund_account_id,
                        plan.start_date,
                        plan.next_trade_date,
                        plan.active,
                        &plan.amount,
                        &plan.fee_rate_bps,
                        &plan.strategy,
                        &plan.index_code,
                        plan.moving_average_days,
                    ))
                    .map_err(err500)?,
                );
            }
            if budget > 0
                && row.share_bps > 0
                && existing.is_none()
                && config.start_date < investments::china_today()
            {
                generation_error = "新增计划的起始日不能在过去，请修改日期并重新保存".into();
            }
            rows.push(PreviewRow {
                key: row.key.clone(),
                name: row.name.clone(),
                share: percent(row.share_bps),
                monthly_target: currency::format(targets[index], &config.currency),
                daily_amount: currency::format(daily, from_currency),
                estimated_monthly: currency::format(
                    product(daily, config.trading_days)?,
                    from_currency,
                ),
                fee_per_day: currency::format(fee, from_currency),
                strategy_label: if row.strategy == "smart" {
                    format!(
                        "聪明定投 · {} · {} 日均线",
                        crate::market_data::index_option(&row.index_code)
                            .map(|option| option.name)
                            .unwrap_or("未知指数"),
                        row.moving_average_days
                    )
                } else {
                    "固定金额".into()
                },
                fund_label: row
                    .fund_account_id
                    .and_then(|id| names.get(&id))
                    .cloned()
                    .unwrap_or_else(|| "确认后新建具体基金（初始价值为 0）".into()),
                current_value: currency::format(values[index], &config.currency),
                target_value: currency::format(holding_targets[index], &config.currency),
                rebalance_gap: currency::format(
                    holding_targets[index] - values[index],
                    &config.currency,
                ),
                plan_id: existing.map(|plan| plan.id),
            });
        }
        if let Some(source) = source {
            if balances[&source.id] < daily_debit {
                warnings.push(format!("当前扣款账户余额不足一次基准定投及手续费：预计需 {}，当前 {}。创建计划不扣款，执行时不足会保留待执行。", currency::format(daily_debit, from_currency), currency::format(balances[&source.id], from_currency)));
            }
        }
        Some(Preview {
            income: currency::format(basis, &config.currency),
            income_source: if config.manual_monthly_income.is_empty() {
                format!("最近 {} 个完整月收入均值", config.income_months)
            } else {
                "手动填写（覆盖收入推算）".into()
            },
            monthly_investment: currency::format(budget, &config.currency),
            monthly_remaining: currency::format(basis - budget, &config.currency),
            emergency_target: if config.emergency_months == 0 {
                "未启用应急金目标".into()
            } else {
                currency::format(goal, &config.currency)
            },
            reserve_current: currency::format(reserve, &config.currency),
            reserve_gap: currency::format(gap, &config.currency),
            reserve_months: format!(
                "{} 个月",
                (Decimal::from(reserve) / Decimal::from(basis)).round_dp(1)
            ),
            portfolio_total: currency::format(portfolio, &config.currency),
            rows,
            can_generate: generation_error.is_empty(),
            generation_error,
            equity_percent: percent(equity),
            currency: config.currency.clone(),
            from_currency: from_currency.into(),
            exchange_info,
        })
    } else {
        if let Some(message) = account_error {
            warnings.push(format!("账户配置需要修正：{message}。请重新选择扣款、基金或应急金账户后保存；当前禁止生成定投，不会自动改写配置。"));
        } else {
            warnings.push("近几个月没有可用于推算的正收入，或汇率无法取得；请手动填写月生活费后保存，不能以 0 或虚构收入生成方案。".into());
        }
        None
    };
    let options = accounts
        .iter()
        .map(|item| AccountOption {
            id: item.id,
            name: names.get(&item.id).cloned().unwrap_or_default(),
            kind: item.kind.clone(),
            currency: item.currency.clone(),
        })
        .collect();
    let data = FinanceData {
        config: saved.config,
        revision,
        accounts: options,
        index_options: crate::market_data::INDEX_OPTIONS
            .iter()
            .map(|item| IndexOption {
                code: item.code,
                name: item.name,
            })
            .collect(),
        moving_average_options: crate::market_data::MOVING_AVERAGE_OPTIONS,
        income,
        preview,
        warnings,
        linked_plans: saved.linked_plans,
        generation_token: URL_SAFE_NO_PAD.encode(token.finalize()),
        notice,
        today: investments::china_today().to_string(),
    };
    Ok(Proposal {
        data,
        daily_amounts,
        plans,
    })
}

fn private_response(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn result_response(headers: &HeaderMap, data: FinanceData) -> Response {
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"))
    {
        private_response(Json(data))
    } else {
        private_response(Redirect::to("/finance"))
    }
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(zone): Extension<ClientTimeZone>,
) -> HandlerResult<Response> {
    let (saved, revision) = load_saved(&state, &dek).await?;
    let data = proposal(&state, &dek, zone, saved, revision, String::new(), true)
        .await?
        .data;
    let data_json = serde_json::to_string(&data)
        .map_err(err500)?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    Ok(private_response(Html(
        FinanceTemplate { data_json }.render().map_err(err500)?,
    )))
}

pub async fn save(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(zone): Extension<ClientTimeZone>,
    headers: HeaderMap,
    Form(form): Form<SaveForm>,
) -> HandlerResult<Response> {
    let _guard = state.balance_writes.lock().await;
    let (previous, revision) = load_saved(&state, &dek).await?;
    if form.revision != revision {
        return Err(conflict());
    }
    let config: Config = serde_json::from_str(&form.config_json)
        .map_err(|error| bad(format!("理财配置格式不正确：{error}")))?;
    let saved = Saved {
        config: normalize(config)?,
        linked_plans: previous.linked_plans,
    };
    let next_revision = revision
        .checked_add(1)
        .ok_or_else(|| err500("配置版本超出范围"))?;
    let payload = crypto::encrypt(&dek, &serde_json::to_vec(&saved).map_err(err500)?);
    let proposal = proposal(
        &state,
        &dek,
        zone,
        saved,
        next_revision,
        "配置已加密保存，预览不会执行定投".into(),
        false,
    )
    .await?;
    if let Some(model) = financial_plan::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        let mut active = model.into_active_model();
        active.payload = Set(payload);
        active.revision = Set(next_revision);
        active.update(&state.db).await.map_err(err500)?;
    } else {
        financial_plan::ActiveModel {
            id: Set(1),
            payload: Set(payload),
            revision: Set(next_revision),
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(result_response(&headers, proposal.data))
}

pub async fn generate(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(zone): Extension<ClientTimeZone>,
    headers: HeaderMap,
    Form(form): Form<GenerateForm>,
) -> HandlerResult<Response> {
    let _guard = state.balance_writes.lock().await;
    let (saved, revision) = load_saved(&state, &dek).await?;
    if revision == 0 || revision != form.revision {
        return Err(conflict());
    }
    let mut proposal = proposal(&state, &dek, zone, saved, revision, String::new(), false).await?;
    if proposal.data.generation_token != form.generation_token {
        return Err(conflict());
    }
    let preview = proposal
        .data
        .preview
        .as_ref()
        .ok_or_else(|| bad("无法推算生活费，请手动填写后保存方案"))?;
    if !preview.can_generate {
        return Err(bad(&preview.generation_error));
    }
    let mut saved = Saved {
        config: proposal.data.config,
        linked_plans: proposal.data.linked_plans,
    };
    let mut has_positive = false;
    for (row, amount) in saved.config.allocations.iter().zip(&proposal.daily_amounts) {
        if row.share_bps > 0 && *amount > 0 {
            has_positive = true;
        }
    }
    let source = if has_positive {
        Some(
            account::Entity::find_by_id(
                saved
                    .config
                    .from_account_id
                    .ok_or_else(|| bad("请选择扣款账户"))?,
            )
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad("扣款账户已删除"))?,
        )
    } else {
        None
    };
    let needs_new_plan = saved
        .config
        .allocations
        .iter()
        .zip(&proposal.daily_amounts)
        .any(|(row, amount)| {
            *amount > 0
                && !saved
                    .linked_plans
                    .iter()
                    .any(|link| link.key == row.key && proposal.plans.contains_key(&link.plan_id))
        });
    let next_trade = if needs_new_plan {
        Some(investments::next_trading_day(&state, saved.config.start_date).await?)
    } else {
        None
    };
    // Resolve calendar reads before the transaction: SQLite may use a single pooled connection.
    for (row, amount) in saved.config.allocations.iter().zip(&proposal.daily_amounts) {
        if *amount == 0 {
            continue;
        }
        let Some(link) = saved.linked_plans.iter().find(|link| link.key == row.key) else {
            continue;
        };
        if let Some(plan) = proposal.plans.get_mut(&link.plan_id) {
            if !plan.active {
                plan.next_trade_date = investments::next_trading_day(
                    &state,
                    plan.next_trade_date
                        .max(plan.start_date)
                        .max(investments::china_today()),
                )
                .await?;
            }
        }
    }
    let tx = state.db.begin().await.map_err(err500)?;
    let current = financial_plan::Entity::find_by_id(1)
        .one(&tx)
        .await
        .map_err(err500)?
        .ok_or_else(conflict)?;
    if current.revision != revision {
        return Err(conflict());
    }
    let mut group_id = saved.config.parent_account_id;
    let mut links = Vec::new();
    let mut retained = HashSet::new();
    for (row, amount) in saved
        .config
        .allocations
        .iter_mut()
        .zip(proposal.daily_amounts.iter().copied())
    {
        if amount == 0 {
            if let Some(link) = saved
                .linked_plans
                .iter()
                .find(|link| link.key == row.key && proposal.plans.contains_key(&link.plan_id))
            {
                links.push(link.clone());
            }
            continue;
        }
        let from = source.as_ref().ok_or_else(|| bad("请选择扣款账户"))?;
        let fund_id = if let Some(id) = row.fund_account_id {
            id
        } else {
            let parent = if let Some(id) = group_id {
                id
            } else {
                let group = account::ActiveModel {
                    name: Set(crypto::encrypt(&dek, saved.config.name.as_bytes())),
                    kind: Set("investment".into()),
                    currency: Set(from.currency.clone()),
                    parent_id: Set(None),
                    balance_offset: Set(crypto::encrypt_cents(&dek, 0)),
                    sms_names: Set(String::new()),
                    note: Set(crypto::encrypt(&dek, "理财方案的基金分组".as_bytes())),
                    created_at: Set(Utc::now()),
                    ..Default::default()
                }
                .insert(&tx)
                .await
                .map_err(err500)?;
                group_id = Some(group.id);
                group.id
            };
            let fund = account::ActiveModel {
                name: Set(crypto::encrypt(&dek, row.name.as_bytes())),
                kind: Set("investment_fund".into()),
                currency: Set(from.currency.clone()),
                parent_id: Set(Some(parent)),
                balance_offset: Set(crypto::encrypt_cents(&dek, 0)),
                sms_names: Set(crypto::encrypt(&dek, b"[]")),
                note: Set(crypto::encrypt(
                    &dek,
                    "由理财方案创建；请定期校准持仓价值".as_bytes(),
                )),
                created_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(&tx)
            .await
            .map_err(err500)?;
            row.fund_account_id = Some(fund.id);
            fund.id
        };
        let old = saved
            .linked_plans
            .iter()
            .find(|link| link.key == row.key)
            .and_then(|link| proposal.plans.remove(&link.plan_id));
        let name = crypto::encrypt(
            &dek,
            format!("{} · {}", saved.config.name, row.name).as_bytes(),
        );
        let note = crypto::encrypt(
            &dek,
            format!(
                "来自理财方案；按 {} 个估算交易日拆分月预算，非银行下单。",
                saved.config.trading_days
            )
            .as_bytes(),
        );
        let plan = if let Some(old) = old {
            let resumed_date = (!old.active).then_some(old.next_trade_date);
            let mut active = old.into_active_model();
            active.name = Set(name);
            active.from_account_id = Set(from.id);
            active.fund_account_id = Set(fund_id);
            active.amount = Set(crypto::encrypt_cents(&dek, amount));
            active.fee_rate_bps = Set(crypto::encrypt_cents(&dek, row.fee_rate_bps));
            active.strategy = Set(row.strategy.clone());
            active.index_code = Set(row.index_code.clone());
            active.moving_average_days = Set(row.moving_average_days);
            if let Some(date) = resumed_date {
                active.next_trade_date = Set(date);
            }
            active.active = Set(true);
            active.note = Set(note);
            active.update(&tx).await.map_err(err500)?
        } else {
            recurring_investment::ActiveModel {
                name: Set(name),
                from_account_id: Set(from.id),
                fund_account_id: Set(fund_id),
                amount: Set(crypto::encrypt_cents(&dek, amount)),
                fee_rate_bps: Set(crypto::encrypt_cents(&dek, row.fee_rate_bps)),
                strategy: Set(row.strategy.clone()),
                index_code: Set(row.index_code.clone()),
                moving_average_days: Set(row.moving_average_days),
                start_date: Set(saved.config.start_date),
                next_trade_date: Set(next_trade.ok_or_else(|| bad("未取得新计划交易日"))?),
                active: Set(true),
                note: Set(note),
                created_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(&tx)
            .await
            .map_err(err500)?
        };
        retained.insert(plan.id);
        links.push(LinkedPlan {
            key: row.key.clone(),
            plan_id: plan.id,
            fund_account_id: fund_id,
        });
    }
    for link in &saved.linked_plans {
        if !retained.contains(&link.plan_id) {
            if let Some(old) = proposal.plans.remove(&link.plan_id) {
                let mut active = old.into_active_model();
                active.active = Set(false);
                active.update(&tx).await.map_err(err500)?;
            }
        }
    }
    saved.config.parent_account_id = group_id;
    saved.linked_plans = links;
    let next_revision = revision
        .checked_add(1)
        .ok_or_else(|| err500("配置版本超出范围"))?;
    let mut active = current.into_active_model();
    active.payload = Set(crypto::encrypt(
        &dek,
        &serde_json::to_vec(&saved).map_err(err500)?,
    ));
    active.revision = Set(next_revision);
    active.update(&tx).await.map_err(err500)?;
    tx.commit().await.map_err(err500)?;
    let data = self::proposal(
        &state,
        &dek,
        zone,
        saved,
        next_revision,
        "定投计划已同步；当前项目已启用，移除/零配比的旧计划已暂停。未生成任何资金流水。".into(),
        false,
    )
    .await?
    .data;
    Ok(result_response(&headers, data))
}
