use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, Redirect},
    Form, Json,
};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday};
use regex::{Captures, Regex};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, str::FromStr, sync::OnceLock};

use crate::{
    crypto,
    entity::{
        account, account_detail, bill, category, investment_execution, investment_sms_event,
        market_closed_day, preference, recurring_investment, transfer,
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

#[derive(Deserialize)]
pub struct SmsWebhookData {
    time: String,
    raw: String,
}

#[derive(Clone)]
pub(crate) enum SmsKind {
    Success {
        amount: i64,
        balance: Option<i64>,
    },
    Failure {
        reason: String,
    },
    BankTransfer {
        recipient: String,
        amount: i64,
        balance: i64,
    },
    QuickPayment {
        description: String,
        amount: i64,
        balance: i64,
    },
}

#[derive(Clone)]
pub(crate) struct PendingSms {
    event_hash: String,
    occurred_at: DateTime<Utc>,
    raw: String,
    sms_date: NaiveDate,
    card_last4: String,
    fund_name: String,
    kind: SmsKind,
}

#[derive(Serialize)]
pub struct SmsWebhookResponse {
    ok: bool,
    status: String,
    message: String,
    pending: usize,
}

#[derive(Serialize)]
pub struct SmsProcessResponse {
    ok: bool,
    processed: usize,
    failures: Vec<String>,
    message: String,
}

fn parse_nonnegative_amount(value: &str, field: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request(format!("{field}格式不正确")))?
        .round_dp(2);
    if decimal < Decimal::ZERO {
        return Err(bad_request(format!("{field}不能小于 0")));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request(format!("{field}超出范围")))
}

fn event_hash(time: &str, raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(time.trim().as_bytes());
    hasher.update(b"\n");
    hasher.update(raw.trim().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sms_date(value: &str, occurred_at: DateTime<Utc>) -> HandlerResult<NaiveDate> {
    let local = occurred_at.with_timezone(&chrono_tz::Asia::Shanghai);
    let expected = format!("{:02}月{:02}日", local.month(), local.day());
    if value != expected {
        return Err(bad_request(format!(
            "短信日期 {value} 与 time 对应的北京时间 {expected} 不一致"
        )));
    }
    Ok(local.date_naive())
}

fn investment_success_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^【招商银行】您尾号(?P<last4>\d{4})的账户于(?P<month>\d{2})月(?P<day>\d{2})日执行「(?P<fund>.+?)」的(?:聪明)?定投计划，扣款(?P<amount>\d+(?:\.\d{1,2})?)元，活期余额(?P<balance>\d+(?:\.\d{1,2})?)元[。.]?$").expect("招商银行定投成功短信正则无效"))
}

fn investment_failure_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^【招商银行】您尾号(?P<last4>\d{4})的账户于(?P<month>\d{2})月(?P<day>\d{2})日存在\d+笔定投计划执行失败，原因：(?P<reason>.+?涉及(?P<fund>[^。，,]+))(?:。查看计划.*)?[。.]?$").expect("招商银行定投失败短信正则无效"))
}

fn bank_transfer_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^【招商银行】您账户(?P<last4>\d{4})于(?P<month>\d{2})月(?P<day>\d{2})日(?P<hour>\d{2}):(?P<minute>\d{2})实时转至他行人民币(?P<amount>\d+(?:\.\d{1,2})?)(?:元)?，余额(?P<balance>\d+(?:\.\d{1,2})?)(?:元)?，收款人(?P<recipient>.+?)[。.]?$").expect("招商银行实时转账短信正则无效"))
}

fn quick_payment_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^【招商银行】您账户(?P<last4>\d{4})于(?P<month>\d{2})月(?P<day>\d{2})日(?P<hour>\d{2}):(?P<minute>\d{2})在(?P<description>.+?)快捷支付(?P<amount>\d+(?:\.\d{1,2})?)元，余额(?P<balance>\d+(?:\.\d{1,2})?)(?:元)?[。.]?$").expect("招商银行快捷支付短信正则无效"))
}

fn capture<'a>(captures: &'a Captures<'a>, name: &str) -> HandlerResult<&'a str> {
    captures
        .name(name)
        .map(|value| value.as_str())
        .ok_or_else(|| bad_request(format!("短信缺少 {name} 字段")))
}

fn validate_captured_time(
    captures: &Captures<'_>,
    occurred_at: DateTime<Utc>,
) -> HandlerResult<NaiveDate> {
    let date_text = format!(
        "{}月{}日",
        capture(captures, "month")?,
        capture(captures, "day")?
    );
    let date = sms_date(&date_text, occurred_at)?;
    if let (Some(hour), Some(minute)) = (captures.name("hour"), captures.name("minute")) {
        let local = occurred_at.with_timezone(&chrono_tz::Asia::Shanghai);
        let sms_hour = hour
            .as_str()
            .parse::<u32>()
            .map_err(|_| bad_request("短信小时格式不正确"))?;
        let sms_minute = minute
            .as_str()
            .parse::<u32>()
            .map_err(|_| bad_request("短信分钟格式不正确"))?;
        if local.hour() != sms_hour || local.minute() != sms_minute {
            return Err(bad_request("短信时分与 time 对应的北京时间不一致"));
        }
    }
    Ok(date)
}

fn parse_sms(data: SmsWebhookData) -> HandlerResult<PendingSms> {
    if data.raw.len() > 4096 {
        return Err(bad_request("raw 短信内容不能超过 4096 字节"));
    }
    let occurred_at = DateTime::parse_from_rfc3339(data.time.trim())
        .map_err(|_| bad_request("time 必须是带时区的 ISO8601 日期时间"))?
        .with_timezone(&Utc);
    let raw = data.raw.trim();
    let (captures, template) = if let Some(captures) = investment_success_regex().captures(raw) {
        (captures, "investment_success")
    } else if let Some(captures) = investment_failure_regex().captures(raw) {
        (captures, "investment_failure")
    } else if let Some(captures) = bank_transfer_regex().captures(raw) {
        (captures, "bank_transfer")
    } else if let Some(captures) = quick_payment_regex().captures(raw) {
        (captures, "quick_payment")
    } else {
        return Err(bad_request("无法匹配已支持的招商银行短信模板"));
    };
    let parsed_date = validate_captured_time(&captures, occurred_at)?;
    let card_last4 = capture(&captures, "last4")?.to_string();
    let (fund_name, kind) = match template {
        "investment_success" => (
            capture(&captures, "fund")?.trim().to_string(),
            SmsKind::Success {
                amount: parse_amount(capture(&captures, "amount")?)?,
                balance: Some(parse_nonnegative_amount(
                    capture(&captures, "balance")?,
                    "短信活期余额",
                )?),
            },
        ),
        "investment_failure" => (
            capture(&captures, "fund")?.trim().to_string(),
            SmsKind::Failure {
                reason: capture(&captures, "reason")?.trim().to_string(),
            },
        ),
        "bank_transfer" => (
            String::new(),
            SmsKind::BankTransfer {
                recipient: capture(&captures, "recipient")?.trim().to_string(),
                amount: parse_amount(capture(&captures, "amount")?)?,
                balance: parse_nonnegative_amount(capture(&captures, "balance")?, "短信余额")?,
            },
        ),
        "quick_payment" => (
            String::new(),
            SmsKind::QuickPayment {
                description: capture(&captures, "description")?.trim().to_string(),
                amount: parse_amount(capture(&captures, "amount")?)?,
                balance: parse_nonnegative_amount(capture(&captures, "balance")?, "短信余额")?,
            },
        ),
        _ => unreachable!(),
    };
    Ok(PendingSms {
        event_hash: event_hash(data.time.trim(), raw),
        occurred_at,
        raw: raw.to_string(),
        sms_date: parsed_date,
        card_last4,
        fund_name,
        kind,
    })
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
    smart: bool,
    manual: bool,
    sms: bool,
    amount_value: String,
    strategy_label: String,
    currency: String,
    start_date: String,
    next_trade_date: String,
    active: bool,
    due: bool,
    note: String,
}

struct SmsEventRow {
    id: i64,
    occurred_at: String,
    plan_name: String,
    detail: String,
    raw: String,
    danger: bool,
    pending: bool,
    repayment: bool,
}

struct ManualDueRow {
    id: i64,
    name: String,
    from_account: String,
    fund_account: String,
    currency: String,
    trade_date: String,
    default_amount: String,
    fee_rate: String,
}

struct ExecutionRow {
    plan_name: String,
    trade_date: String,
    from_account: String,
    fund_account: String,
    amount: String,
    fee: String,
    strategy: String,
    decision: String,
}

struct ClosedDayRow {
    date: String,
    name: String,
}

struct MovingAverageOption {
    days: i32,
    selected: bool,
}

fn moving_average_options(selected: i32) -> Vec<MovingAverageOption> {
    crate::market_data::MOVING_AVERAGE_OPTIONS
        .iter()
        .map(|days| MovingAverageOption {
            days: *days,
            selected: *days == selected,
        })
        .collect()
}

#[derive(Template)]
#[template(path = "investments.html")]
struct InvestmentsTemplate {
    plans: Vec<PlanRow>,
    manual_due: Vec<ManualDueRow>,
    sms_events: Vec<SmsEventRow>,
    pending_sms_count: usize,
    transfer_targets: Vec<AccountOption>,
    repayment_targets: Vec<AccountOption>,
    executions: Vec<ExecutionRow>,
    custom_closed_days: Vec<ClosedDayRow>,
    keyword: String,
    status: String,
    per_page: usize,
    pagination: super::PaginationView,
    due_count: usize,
    automatic_due_count: usize,
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
    strategy: String,
    index_code: String,
    sms_fund_name: String,
    from_account_id: i64,
    fund_account_id: i64,
    start_date: String,
    note: String,
    active: bool,
    min_start_date: String,
    source_accounts: Vec<AccountOption>,
    fund_accounts: Vec<AccountOption>,
    index_options: &'static [crate::market_data::IndexOption],
    moving_average_options: Vec<MovingAverageOption>,
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
    #[serde(default)]
    strategy: String,
    #[serde(default)]
    index_code: String,
    #[serde(default)]
    sms_fund_name: String,
    #[serde(default)]
    moving_average_days: String,
    from_account_id: i64,
    fund_account_id: i64,
    start_date: String,
    note: String,
    #[serde(default)]
    active: Option<String>,
}

#[derive(Deserialize)]
pub struct ManualExecuteFormData {
    trade_date: String,
    amount: String,
}

#[derive(Deserialize)]
pub struct SmsConfirmFormData {
    to_account_id: i64,
}

struct ParsedInvestment {
    name: String,
    amount: i64,
    fee_rate_bps: i64,
    strategy: String,
    index_code: String,
    sms_fund_name: String,
    moving_average_days: i32,
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
    warnings: Vec<String>,
    message: String,
}

#[derive(Deserialize)]
pub struct SmartPreviewQuery {
    index_code: String,
    moving_average_days: i32,
    #[serde(default)]
    trade_date: String,
}

#[derive(Serialize)]
pub struct SmartPreviewResponse {
    ok: bool,
    index_name: String,
    moving_average_days: i32,
    quote_date: String,
    close: String,
    moving_average: String,
    deviation: String,
    multiplier: String,
    source: String,
    warning: String,
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

async fn parse_form(
    state: &AppState,
    dek: &crypto::Dek,
    form: InvestmentFormData,
) -> HandlerResult<ParsedInvestment> {
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
    let strategy = match form.strategy.as_str() {
        "fixed" => "fixed",
        "smart" => "smart",
        "manual" => "manual",
        "sms" => "sms",
        _ => return Err(bad_request("请选择支持的扣款策略")),
    };
    let (index_code, moving_average_days) = if strategy == "smart" {
        if crate::market_data::index_option(form.index_code.trim()).is_none() {
            return Err(bad_request("请选择支持的跟踪指数"));
        }
        let days = form
            .moving_average_days
            .parse::<i32>()
            .map_err(|_| bad_request("均线周期格式不正确"))?;
        if !crate::market_data::valid_moving_average(days) {
            return Err(bad_request("请选择支持的均线周期"));
        }
        (form.index_code.trim().to_string(), days)
    } else {
        (String::new(), 180)
    };
    let sms_fund_name = if strategy == "sms" {
        if from_account.currency != "CNY" {
            return Err(bad_request("招商银行短信实扣模式目前只支持 CNY 账户"));
        }
        let value = form.sms_fund_name.trim();
        if value.is_empty() {
            return Err(bad_request("短信实扣模式必须填写短信中的基金名称"));
        }
        let detail = account_detail::Entity::find_by_id(from_account.id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("短信实扣模式的扣款账户必须配置银行卡号"))?;
        let card_number = crypto::decrypt_string(dek, &detail.card_number);
        if card_number.chars().filter(|ch| ch.is_ascii_digit()).count() < 4 {
            return Err(bad_request("短信实扣模式的扣款账户必须配置有效银行卡号"));
        }
        value.to_string()
    } else {
        String::new()
    };
    let amount = if strategy == "sms" {
        0
    } else if strategy == "manual" && form.amount.trim().is_empty() {
        0
    } else {
        parse_amount(&form.amount)?
    };
    let fee_rate_bps = if strategy == "sms" {
        0
    } else {
        parse_fee_rate_bps(&form.fee_rate)?
    };
    Ok(ParsedInvestment {
        name: name.into(),
        amount,
        fee_rate_bps,
        strategy: strategy.into(),
        index_code,
        sms_fund_name,
        moving_average_days,
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
    let transfer_targets = accounts
        .iter()
        .filter(|account| account.currency == "CNY")
        .cloned()
        .collect();
    let repayment_targets = accounts
        .iter()
        .filter(|account| {
            account.currency == "CNY"
                && matches!(account.kind.as_str(), "credit_card" | "credit_service")
        })
        .cloned()
        .collect();
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
            let smart = plan.strategy == "smart";
            let manual = plan.strategy == "manual";
            let sms = plan.strategy == "sms";
            let strategy_label = match plan.strategy.as_str() {
                "smart" => {
                    let index_name = crate::market_data::index_option(&plan.index_code)
                        .map(|item| item.name)
                        .unwrap_or("未知指数");
                    format!(
                        "聪明定投 · {index_name} · {}日均线",
                        plan.moving_average_days
                    )
                }
                "manual" => "每日手动填写金额".into(),
                "sms" => "招商银行短信实扣金额".into(),
                _ => "固定金额".into(),
            };
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
            let amount = crypto::decrypt_cents(&dek, &plan.amount);
            PlanRow {
                id: plan.id,
                name,
                from_account,
                fund_account,
                amount: if sms {
                    "以短信为准".into()
                } else if manual && amount == 0 {
                    "每日填写".into()
                } else {
                    crate::currency::format(amount, &currency)
                },
                fee_rate: format_fee_rate(crypto::decrypt_cents(&dek, &plan.fee_rate_bps)),
                smart,
                manual,
                sms,
                amount_value: if amount > 0 {
                    super::fmt_cents(amount)
                } else {
                    String::new()
                },
                strategy_label,
                currency,
                start_date: plan.start_date.format("%Y-%m-%d").to_string(),
                next_trade_date: if sms {
                    "等待短信".into()
                } else {
                    plan.next_trade_date.format("%Y-%m-%d").to_string()
                },
                active: plan.active,
                due: plan.active && !sms && plan.next_trade_date <= today,
                note,
            }
        })
        .collect::<Vec<_>>();
    let active_count = plan_rows.iter().filter(|plan| plan.active).count();
    let due_count = plan_rows.iter().filter(|plan| plan.due).count();
    let automatic_due_count = plan_rows
        .iter()
        .filter(|plan| plan.due && !plan.manual && !plan.sms)
        .count();
    let manual_due = plan_rows
        .iter()
        .filter(|plan| plan.due && plan.manual)
        .map(|plan| ManualDueRow {
            id: plan.id,
            name: plan.name.clone(),
            from_account: plan.from_account.clone(),
            fund_account: plan.fund_account.clone(),
            currency: plan.currency.clone(),
            trade_date: plan.next_trade_date.clone(),
            default_amount: plan.amount_value.clone(),
            fee_rate: plan.fee_rate.clone(),
        })
        .collect();
    let mut plans = plan_rows
        .into_iter()
        .filter(|row| {
            let matches_keyword = keyword.is_empty()
                || format!(
                    "{} {} {} {} {} {}",
                    row.name,
                    row.from_account,
                    row.fund_account,
                    row.note,
                    row.currency,
                    row.strategy_label
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
    let sms_events = investment_sms_event::Entity::find()
        .order_by_desc(investment_sms_event::Column::OccurredAt)
        .order_by_desc(investment_sms_event::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|event| {
            let plan_name = event
                .plan_id
                .and_then(|id| all_plans.get(&id).cloned())
                .unwrap_or_else(|| match event.kind.as_str() {
                    "bank_transfer" => "本人账户转账".into(),
                    "quick_payment" => "信用卡还款".into(),
                    _ => "未匹配计划".into(),
                });
            SmsEventRow {
                id: event.id,
                occurred_at: event.occurred_at.format("%Y-%m-%dT%H:%M").to_string(),
                plan_name,
                detail: crypto::decrypt_string(&dek, &event.detail),
                raw: crypto::decrypt_string(&dek, &event.raw),
                danger: matches!(event.status.as_str(), "failed" | "unmatched" | "error"),
                pending: event.status == "pending",
                repayment: event.kind == "quick_payment",
            }
        })
        .take(20)
        .collect();
    let pending_sms_count = state.pending_sms.lock().await.len();
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
            let (strategy, decision) = if execution.strategy == "sms" {
                (
                    "短信实扣".into(),
                    "按招商银行短信中的实际扣款金额执行".into(),
                )
            } else if execution.strategy == "manual" {
                ("手动金额".into(), "当日由用户确认后执行".into())
            } else if execution.index_code.is_empty() {
                ("固定金额".into(), String::new())
            } else {
                let index_name = crate::market_data::index_option(&execution.index_code)
                    .map(|item| item.name)
                    .unwrap_or("未知指数");
                let close = Decimal::from_str(&execution.index_close).unwrap_or_default();
                let average = Decimal::from_str(&execution.moving_average).unwrap_or_default();
                let deviation = if average > Decimal::ZERO {
                    (close - average) * Decimal::from(100) / average
                } else {
                    Decimal::ZERO
                };
                let multiplier = crypto::decrypt_cents(&dek, &execution.multiplier_bps);
                let base_amount = crypto::decrypt_cents(&dek, &execution.base_amount);
                (
                    format!("聪明 · {index_name} · {}日", execution.moving_average_days),
                    format!(
                        "{} 收盘 {} / 均线 {}（{}）· 基准 {} × {}",
                        execution
                            .quote_date
                            .map(|date| date.format("%Y-%m-%d").to_string())
                            .unwrap_or_else(|| "未知日期".into()),
                        crate::market_data::format_point(close),
                        crate::market_data::format_point(average),
                        crate::market_data::format_percent(deviation),
                        crate::currency::format(base_amount, &currency),
                        crate::market_data::format_multiplier(multiplier),
                    ),
                )
            };
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
                strategy,
                decision,
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
        manual_due,
        sms_events,
        pending_sms_count,
        transfer_targets,
        repayment_targets,
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
        automatic_due_count,
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
        strategy: "fixed".into(),
        index_code: "000300".into(),
        sms_fund_name: String::new(),
        from_account_id: 0,
        fund_account_id: 0,
        start_date: today.clone(),
        note: String::new(),
        active: true,
        min_start_date: today,
        source_accounts,
        fund_accounts,
        index_options: crate::market_data::INDEX_OPTIONS,
        moving_average_options: moving_average_options(180),
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
    let parsed = parse_form(&state, &dek, form).await?;
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
        strategy: Set(parsed.strategy),
        index_code: Set(parsed.index_code),
        moving_average_days: Set(parsed.moving_average_days),
        sms_fund_name: Set(crypto::encrypt(&dek, parsed.sms_fund_name.as_bytes())),
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
    let stored_amount = crypto::decrypt_cents(&dek, &plan.amount);
    let html = InvestmentFormTemplate {
        heading: "编辑每日定投".into(),
        action: format!("/investments/{id}/edit"),
        name: crypto::decrypt_string(&dek, &plan.name),
        amount: if plan.strategy == "manual" && stored_amount == 0 {
            String::new()
        } else {
            super::fmt_cents(stored_amount)
        },
        fee_rate: super::fmt_cents(crypto::decrypt_cents(&dek, &plan.fee_rate_bps)),
        strategy: plan.strategy.clone(),
        index_code: plan.index_code.clone(),
        sms_fund_name: crypto::decrypt_string(&dek, &plan.sms_fund_name),
        from_account_id: plan.from_account_id,
        fund_account_id: plan.fund_account_id,
        start_date: start_date.clone(),
        note: crypto::decrypt_string(&dek, &plan.note),
        active: plan.active,
        min_start_date: start_date,
        source_accounts,
        fund_accounts,
        index_options: crate::market_data::INDEX_OPTIONS,
        moving_average_options: moving_average_options(plan.moving_average_days),
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
    let parsed = parse_form(&state, &dek, form).await?;
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
    active.strategy = Set(parsed.strategy);
    active.index_code = Set(parsed.index_code);
    active.moving_average_days = Set(parsed.moving_average_days);
    active.sms_fund_name = Set(crypto::encrypt(&dek, parsed.sms_fund_name.as_bytes()));
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
    manual_amount: Option<i64>,
    happened_at_override: Option<NaiveDateTime>,
) -> HandlerResult<NaiveDate> {
    if plan.strategy != "sms" && !is_trading_day(state, trade_date).await? {
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
        let next_start = trade_date
            .succ_opt()
            .ok_or_else(|| bad_request("下一日期超出范围"))?;
        let next_date = if plan.strategy == "sms" {
            next_start
        } else {
            next_trading_day(state, next_start).await?
        };
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
    let base_amount = if matches!(plan.strategy.as_str(), "manual" | "sms") {
        manual_amount.ok_or_else(|| bad_request("手动金额计划需要填写本期定投金额"))?
    } else {
        crypto::decrypt_cents(dek, &plan.amount)
    };
    let smart_decision = if plan.strategy == "smart" {
        Some(
            crate::market_data::smart_decision(
                state,
                &plan.index_code,
                plan.moving_average_days,
                trade_date,
            )
            .await
            .map_err(bad_request)?,
        )
    } else {
        None
    };
    let multiplier_bps = smart_decision
        .as_ref()
        .map(|decision| decision.multiplier_bps)
        .unwrap_or(10_000);
    let amount =
        crate::market_data::adjusted_amount(base_amount, multiplier_bps).map_err(bad_request)?;
    let fee_rate_bps = if plan.strategy == "sms" {
        0
    } else {
        crypto::decrypt_cents(dek, &plan.fee_rate_bps)
    };
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
    let next_start = trade_date
        .succ_opt()
        .ok_or_else(|| bad_request("下一日期超出范围"))?;
    let next_date = if plan.strategy == "sms" {
        next_start
    } else {
        next_trading_day(state, next_start).await?
    };
    // 交易日流水记在北京时间 15:00，对应 UTC 07:00；数据库仍只存 UTC。
    let happened_at = happened_at_override.unwrap_or(
        trade_date
            .and_hms_opt(7, 0, 0)
            .ok_or_else(|| bad_request("定投交易时间无效"))?,
    );
    let plan_name = crypto::decrypt_string(dek, &plan.name);
    let transfer_note = if let Some(decision) = &smart_decision {
        format!(
            "聪明定投 · {plan_name} · {} {} · {}",
            decision.index_name,
            decision.moving_average_days,
            crate::market_data::format_multiplier(decision.multiplier_bps)
        )
    } else if plan.strategy == "sms" {
        format!("招商银行短信定投 · {plan_name}")
    } else if plan.strategy == "manual" {
        format!("手动金额定投 · {plan_name}")
    } else {
        format!("每日定投 · {plan_name}")
    };
    let transaction = state.db.begin().await.map_err(err500)?;
    let transfer = transfer::ActiveModel {
        from_account_id: Set(plan.from_account_id),
        to_account_id: Set(plan.fund_account_id),
        amount: Set(crypto::encrypt_cents(dek, amount)),
        to_amount: Set(crypto::encrypt_cents(dek, amount)),
        note: Set(crypto::encrypt(dek, transfer_note.as_bytes())),
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
        base_amount: Set(crypto::encrypt_cents(dek, base_amount)),
        multiplier_bps: Set(crypto::encrypt_cents(dek, multiplier_bps)),
        strategy: Set(plan.strategy.clone()),
        index_code: Set(smart_decision
            .as_ref()
            .map(|decision| decision.index_code.clone())
            .unwrap_or_default()),
        moving_average_days: Set(smart_decision
            .as_ref()
            .map(|decision| decision.moving_average_days)
            .unwrap_or_default()),
        quote_date: Set(smart_decision.as_ref().map(|decision| decision.quote_date)),
        index_close: Set(smart_decision
            .as_ref()
            .map(|decision| decision.close.normalize().to_string())
            .unwrap_or_default()),
        moving_average: Set(smart_decision
            .as_ref()
            .map(|decision| decision.moving_average.normalize().to_string())
            .unwrap_or_default()),
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

pub async fn execute_manual(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<ManualExecuteFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let trade_date = NaiveDate::parse_from_str(form.trade_date.trim(), "%Y-%m-%d")
        .map_err(|_| bad_request("待执行交易日格式不正确"))?;
    let amount = parse_amount(&form.amount)?;
    let plan = recurring_investment::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "定投计划不存在".into()))?;
    if plan.strategy != "manual" {
        return Err(bad_request("这个计划不是每日手动金额策略"));
    }
    if !plan.active {
        return Err(bad_request("这个定投计划已暂停"));
    }
    if trade_date != plan.next_trade_date {
        return Err(bad_request("待执行日期已经变化，请刷新页面后重新填写"));
    }
    if trade_date > china_today() {
        return Err(bad_request("尚未到这个交易日，不能提前执行"));
    }
    execute_plan_day(&state, &dek, &plan, trade_date, Some(amount), None).await?;
    Ok(Redirect::to("/investments"))
}

struct SmsOutcome {
    ok: bool,
    status: String,
    message: String,
}

async fn record_sms_event(
    state: &AppState,
    dek: &crypto::Dek,
    sms: &PendingSms,
    plan_id: Option<i64>,
    status: &str,
    detail: &str,
) -> HandlerResult<()> {
    if let Some(existing) = investment_sms_event::Entity::find()
        .filter(investment_sms_event::Column::EventHash.eq(&sms.event_hash))
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        let mut active = existing.into_active_model();
        active.occurred_at = Set(sms.occurred_at);
        active.kind = Set(match &sms.kind {
            SmsKind::Success { .. } => "success",
            SmsKind::Failure { .. } => "failure",
            SmsKind::BankTransfer { .. } => "bank_transfer",
            SmsKind::QuickPayment { .. } => "quick_payment",
        }
        .into());
        active.status = Set(status.into());
        active.plan_id = Set(plan_id);
        active.raw = Set(crypto::encrypt(dek, sms.raw.as_bytes()));
        active.detail = Set(crypto::encrypt(dek, detail.as_bytes()));
        active.update(&state.db).await.map_err(err500)?;
        return Ok(());
    }
    investment_sms_event::ActiveModel {
        event_hash: Set(sms.event_hash.clone()),
        occurred_at: Set(sms.occurred_at),
        kind: Set(match &sms.kind {
            SmsKind::Success { .. } => "success",
            SmsKind::Failure { .. } => "failure",
            SmsKind::BankTransfer { .. } => "bank_transfer",
            SmsKind::QuickPayment { .. } => "quick_payment",
        }
        .into()),
        status: Set(status.into()),
        plan_id: Set(plan_id),
        raw: Set(crypto::encrypt(dek, sms.raw.as_bytes())),
        detail: Set(crypto::encrypt(dek, detail.as_bytes())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(())
}

fn normalized_sms_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

async fn sms_plan_matches(
    state: &AppState,
    dek: &crypto::Dek,
    plan: &recurring_investment::Model,
    sms: &PendingSms,
) -> HandlerResult<bool> {
    let configured = normalized_sms_name(&crypto::decrypt_string(dek, &plan.sms_fund_name));
    let incoming = normalized_sms_name(&sms.fund_name);
    if configured.is_empty() || !(configured.contains(&incoming) || incoming.contains(&configured))
    {
        return Ok(false);
    }
    let detail = account_detail::Entity::find_by_id(plan.from_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let Some(detail) = detail else {
        return Ok(false);
    };
    let digits = crypto::decrypt_string(dek, &detail.card_number)
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    Ok(digits.ends_with(&sms.card_last4))
}

async fn process_one_sms(
    state: &AppState,
    dek: &crypto::Dek,
    sms: &PendingSms,
) -> HandlerResult<SmsOutcome> {
    if let Some(existing) = investment_sms_event::Entity::find()
        .filter(investment_sms_event::Column::EventHash.eq(&sms.event_hash))
        .one(&state.db)
        .await
        .map_err(err500)?
    {
        if !matches!(existing.status.as_str(), "error" | "unmatched") {
            return Ok(SmsOutcome {
                ok: true,
                status: "duplicate".into(),
                message: "这条短信已经处理过，没有重复记账".into(),
            });
        }
    }
    if let SmsKind::BankTransfer {
        recipient,
        amount,
        balance,
    } = &sms.kind
    {
        let owner_name = preference::Entity::find_by_id(1)
            .one(&state.db)
            .await
            .map_err(err500)?
            .map(|item| crypto::decrypt_string(dek, &item.owner_name))
            .unwrap_or_default();
        let (status, detail, ok) = if owner_name.trim().is_empty() {
            (
                "unmatched",
                "识别到实时转账短信；请先在设置中填写本人姓名，再判断是否为本人账户互转"
                    .to_string(),
                false,
            )
        } else if recipient.trim() == owner_name.trim() {
            (
                "pending",
                format!(
                    "已识别本人账户转账候选：转给 {recipient}，金额 {} 元，短信余额 {}；请选择转入账户后确认",
                    super::fmt_cents(*amount),
                    super::fmt_cents(*balance)
                ),
                true,
            )
        } else {
            (
                "unmatched",
                format!(
                    "识别到转给 {recipient} 的实时转账 {} 元；收款人与本人姓名不一致，未自动记账",
                    super::fmt_cents(*amount)
                ),
                false,
            )
        };
        record_sms_event(state, dek, sms, None, status, &detail).await?;
        return Ok(SmsOutcome {
            ok,
            status: status.into(),
            message: detail,
        });
    }
    if let SmsKind::QuickPayment {
        description,
        amount,
        balance,
    } = &sms.kind
    {
        let owner_name = preference::Entity::find_by_id(1)
            .one(&state.db)
            .await
            .map_err(err500)?
            .map(|item| crypto::decrypt_string(dek, &item.owner_name))
            .unwrap_or_default();
        let repayment_name = description
            .split("信用卡还款-")
            .nth(1)
            .map(str::trim)
            .unwrap_or_default();
        let (status, detail, ok) = if owner_name.trim().is_empty() {
            (
                "unmatched",
                "识别到快捷支付短信；请先在设置中填写本人姓名，再判断是否为本人信用卡还款"
                    .to_string(),
                false,
            )
        } else if description.contains("信用卡还款-") && repayment_name == owner_name.trim() {
            (
                "pending",
                format!(
                    "已识别本人信用卡还款候选：{} 元，短信余额 {}；请选择还款进入的信用账户后确认",
                    super::fmt_cents(*amount),
                    super::fmt_cents(*balance)
                ),
                true,
            )
        } else {
            (
                "unmatched",
                format!(
                    "识别到快捷支付“{description}” {} 元，但不能确认是本人信用卡还款，未自动记账",
                    super::fmt_cents(*amount)
                ),
                false,
            )
        };
        record_sms_event(state, dek, sms, None, status, &detail).await?;
        return Ok(SmsOutcome {
            ok,
            status: status.into(),
            message: detail,
        });
    }
    let plans = recurring_investment::Entity::find()
        .filter(recurring_investment::Column::Active.eq(true))
        .filter(recurring_investment::Column::Strategy.eq("sms"))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut matched = Vec::new();
    for plan in plans {
        if sms_plan_matches(state, dek, &plan, sms).await? {
            matched.push(plan);
        }
    }
    if matched.is_empty() {
        let detail = format!(
            "未找到同时匹配尾号 {} 和基金“{}”的短信实扣计划",
            sms.card_last4, sms.fund_name
        );
        record_sms_event(state, dek, sms, None, "unmatched", &detail).await?;
        return Ok(SmsOutcome {
            ok: false,
            status: "unmatched".into(),
            message: detail,
        });
    }
    if matched.len() > 1 {
        let detail = format!(
            "有多个计划同时匹配尾号 {} 和基金“{}”，请修改短信基金名称使其唯一",
            sms.card_last4, sms.fund_name
        );
        record_sms_event(state, dek, sms, None, "error", &detail).await?;
        return Ok(SmsOutcome {
            ok: false,
            status: "error".into(),
            message: detail,
        });
    }
    let plan = matched.remove(0);
    let plan_name = crypto::decrypt_string(dek, &plan.name);
    if let SmsKind::Failure { reason } = &sms.kind {
        let detail = format!("{plan_name} 扣款失败：{reason}");
        record_sms_event(state, dek, sms, Some(plan.id), "failed", &detail).await?;
        return Ok(SmsOutcome {
            ok: false,
            status: "failed".into(),
            message: detail,
        });
    }
    if investment_execution::Entity::find()
        .filter(investment_execution::Column::PlanId.eq(plan.id))
        .filter(investment_execution::Column::TradeDate.eq(sms.sms_date))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        let detail = format!(
            "{plan_name} 在 {} 已经记过账，本条短信不再重复扣款",
            sms.sms_date
        );
        record_sms_event(state, dek, sms, Some(plan.id), "duplicate", &detail).await?;
        return Ok(SmsOutcome {
            ok: true,
            status: "duplicate".into(),
            message: detail,
        });
    }
    let SmsKind::Success { amount, balance } = &sms.kind else {
        unreachable!();
    };
    let result = execute_plan_day(
        state,
        dek,
        &plan,
        sms.sms_date,
        Some(*amount),
        Some(sms.occurred_at.naive_utc()),
    )
    .await;
    match result {
        Ok(_) => {
            let balance = balance
                .map(|value| format!("；短信报告活期余额 {}", super::fmt_cents(value)))
                .unwrap_or_default();
            let detail = format!(
                "{plan_name} 已按短信实扣 {} 元记账{balance}",
                super::fmt_cents(*amount)
            );
            record_sms_event(state, dek, sms, Some(plan.id), "executed", &detail).await?;
            Ok(SmsOutcome {
                ok: true,
                status: "executed".into(),
                message: detail,
            })
        }
        Err((_, error)) => {
            let detail = format!("{plan_name} 短信已收到，但记账失败：{error}");
            record_sms_event(state, dek, sms, Some(plan.id), "error", &detail).await?;
            Ok(SmsOutcome {
                ok: false,
                status: "error".into(),
                message: detail,
            })
        }
    }
}

pub async fn receive_sms(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(data): Json<SmsWebhookData>,
) -> HandlerResult<(StatusCode, Json<SmsWebhookResponse>)> {
    let token = headers
        .get(header::AUTHORIZATION)
        .or_else(|| headers.get("authentication"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !state.sms_token_matches(token).await.map_err(err500)? {
        return Err((StatusCode::UNAUTHORIZED, "短信接口认证失败".into()));
    }
    let sms = parse_sms(data)?;
    if let Some(dek) = state.any_session_dek() {
        let _balance_guard = state.balance_writes.lock().await;
        let outcome = process_one_sms(&state, &dek, &sms).await?;
        let pending = state.pending_sms.lock().await.len();
        return Ok((
            StatusCode::OK,
            Json(SmsWebhookResponse {
                ok: outcome.ok,
                status: outcome.status,
                message: outcome.message,
                pending,
            }),
        ));
    }
    let mut pending = state.pending_sms.lock().await;
    if !pending.iter().any(|item| item.event_hash == sms.event_hash) {
        if pending.len() >= 100 {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "未解锁短信队列已满，请先解锁 Haruka 处理现有短信".into(),
            ));
        }
        pending.push_back(sms);
    }
    let count = pending.len();
    Ok((
        StatusCode::ACCEPTED,
        Json(SmsWebhookResponse {
            ok: true,
            status: "queued".into(),
            message: "账本当前未解锁，短信已暂存在服务内存中，解锁后自动处理".into(),
            pending: count,
        }),
    ))
}

pub async fn process_pending_sms(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Json<SmsProcessResponse>> {
    let mut items = {
        let mut pending = state.pending_sms.lock().await;
        std::mem::take(&mut *pending)
    };
    let retryable = investment_sms_event::Entity::find()
        .filter(investment_sms_event::Column::Status.is_in(["error", "unmatched"]))
        .all(&state.db)
        .await
        .map_err(err500)?;
    for event in retryable {
        if items.iter().any(|item| item.event_hash == event.event_hash) {
            continue;
        }
        let data = SmsWebhookData {
            time: event.occurred_at.to_rfc3339(),
            raw: crypto::decrypt_string(&dek, &event.raw),
        };
        if let Ok(mut sms) = parse_sms(data) {
            sms.event_hash = event.event_hash;
            items.push_back(sms);
        }
    }
    if items.is_empty() {
        return Ok(Json(SmsProcessResponse {
            ok: true,
            processed: 0,
            failures: Vec::new(),
            message: "没有待处理的短信".into(),
        }));
    }
    let _balance_guard = state.balance_writes.lock().await;
    let mut processed = 0usize;
    let mut failures = Vec::new();
    let mut retry = Vec::new();
    for item in items {
        match process_one_sms(&state, &dek, &item).await {
            Ok(outcome) => {
                processed += 1;
                if !outcome.ok {
                    failures.push(outcome.message);
                }
            }
            Err((_, error)) => {
                failures.push(format!("短信暂时无法处理：{error}"));
                retry.push(item);
            }
        }
    }
    if !retry.is_empty() {
        let mut pending = state.pending_sms.lock().await;
        pending.extend(retry);
    }
    let ok = failures.is_empty();
    let message = if ok {
        format!("已处理 {processed} 条定投短信")
    } else {
        format!(
            "已处理 {processed} 条短信，其中 {} 条需要关注",
            failures.len()
        )
    };
    Ok(Json(SmsProcessResponse {
        ok,
        processed,
        failures,
        message,
    }))
}

async fn sms_source_account(
    state: &AppState,
    dek: &crypto::Dek,
    last4: &str,
) -> HandlerResult<account::Model> {
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut matches = Vec::new();
    for account in accounts {
        let detail = account_detail::Entity::find_by_id(account.id)
            .one(&state.db)
            .await
            .map_err(err500)?;
        let Some(detail) = detail else {
            continue;
        };
        let digits = crypto::decrypt_string(dek, &detail.card_number)
            .chars()
            .filter(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.ends_with(last4) {
            matches.push(account);
        }
    }
    match matches.len() {
        0 => Err(bad_request(format!("没有找到尾号 {last4} 的账户"))),
        1 => Ok(matches.remove(0)),
        _ => Err(bad_request(format!(
            "有多个账户的卡号尾号都是 {last4}，无法安全确认短信来源"
        ))),
    }
}

pub async fn confirm_sms_transfer(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<SmsConfirmFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let event = investment_sms_event::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "短信记录不存在".into()))?;
    if event.status != "pending" {
        return Err(bad_request("这条短信当前不需要确认，或已经处理过"));
    }
    let mut sms = parse_sms(SmsWebhookData {
        time: event.occurred_at.to_rfc3339(),
        raw: crypto::decrypt_string(&dek, &event.raw),
    })?;
    sms.event_hash = event.event_hash.clone();
    let from_account = sms_source_account(&state, &dek, &sms.card_last4).await?;
    let to_account = account::Entity::find_by_id(form.to_account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("选择的转入账户不存在"))?;
    if from_account.id == to_account.id {
        return Err(bad_request("转出账户和转入账户不能相同"));
    }
    if from_account.currency != "CNY" || to_account.currency != "CNY" {
        return Err(bad_request("人民币短信只能确认到 CNY 账户"));
    }
    if matches!(from_account.kind.as_str(), "credit_card" | "credit_service") {
        return Err(bad_request("短信中的扣款来源不能是信用账户"));
    }
    let owner_name = preference::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .map(|item| crypto::decrypt_string(&dek, &item.owner_name))
        .unwrap_or_default();
    let (amount, note) = match &sms.kind {
        SmsKind::BankTransfer {
            recipient, amount, ..
        } => {
            if owner_name.trim().is_empty() || recipient.trim() != owner_name.trim() {
                return Err(bad_request("短信收款人与当前配置的本人姓名不一致"));
            }
            (
                *amount,
                format!("招商银行短信确认 · 本人账户转账 · {recipient}"),
            )
        }
        SmsKind::QuickPayment {
            description,
            amount,
            ..
        } => {
            let repayment_name = description
                .split("信用卡还款-")
                .nth(1)
                .map(str::trim)
                .unwrap_or_default();
            if owner_name.trim().is_empty()
                || repayment_name != owner_name.trim()
                || !description.contains("信用卡还款-")
            {
                return Err(bad_request("这条快捷支付短信不能确认为本人信用卡还款"));
            }
            if !matches!(to_account.kind.as_str(), "credit_card" | "credit_service") {
                return Err(bad_request("信用卡还款只能确认到信用卡或信贷服务账户"));
            }
            (*amount, format!("招商银行短信确认 · {description}"))
        }
        _ => return Err(bad_request("这条短信不是可确认的本人转账或信用卡还款")),
    };
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        from_account.id,
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("短信金额超出范围"))?,
    )
    .await?;
    super::accounts::ensure_balance_delta(&state, &dek, to_account.id, amount).await?;
    let transaction = state.db.begin().await.map_err(err500)?;
    let transfer = transfer::ActiveModel {
        from_account_id: Set(from_account.id),
        to_account_id: Set(to_account.id),
        amount: Set(crypto::encrypt_cents(&dek, amount)),
        to_amount: Set(crypto::encrypt_cents(&dek, amount)),
        note: Set(crypto::encrypt(&dek, note.as_bytes())),
        happened_at: Set(sms.occurred_at.naive_utc()),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    let detail = format!(
        "已确认从 {} 转入 {}，金额 {} 元",
        crypto::decrypt_string(&dek, &from_account.name),
        crypto::decrypt_string(&dek, &to_account.name),
        super::fmt_cents(amount)
    );
    let mut active = event.into_active_model();
    active.status = Set("confirmed".into());
    active.transfer_id = Set(Some(transfer.id));
    active.detail = Set(crypto::encrypt(&dek, detail.as_bytes()));
    active.update(&transaction).await.map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to("/investments"))
}

pub async fn run_due(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Json<RunDueResponse>> {
    let _balance_guard = state.balance_writes.lock().await;
    let today = china_today();
    let plans = recurring_investment::Entity::find()
        .filter(recurring_investment::Column::Active.eq(true))
        .filter(recurring_investment::Column::Strategy.ne("manual"))
        .filter(recurring_investment::Column::Strategy.ne("sms"))
        .filter(recurring_investment::Column::NextTradeDate.lte(today))
        .order_by_asc(recurring_investment::Column::NextTradeDate)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut executed = 0usize;
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    for mut plan in plans {
        let plan_name = crypto::decrypt_string(&dek, &plan.name);
        let mut per_plan = 0usize;
        if plan.strategy == "smart" {
            let history_start = plan.next_trade_date
                - Duration::days(i64::from(plan.moving_average_days.max(1)) * 2 + 60);
            let history_end = today
                .pred_opt()
                .ok_or_else(|| bad_request("指数行情结束日期超出范围"))?;
            if let Err(error) = crate::market_data::refresh_index(
                &state,
                &plan.index_code,
                history_start,
                history_end,
            )
            .await
            {
                match crate::market_data::smart_decision(
                    &state,
                    &plan.index_code,
                    plan.moving_average_days,
                    plan.next_trade_date,
                )
                .await
                {
                    Ok(decision) => warnings.push(format!(
                        "{plan_name}：行情更新失败，改用缓存至 {}（{error}）",
                        decision.quote_date
                    )),
                    Err(cache_error) => {
                        failures.push(format!(
                            "{plan_name}：行情更新失败且缓存不可用：{error}；{cache_error}"
                        ));
                        continue;
                    }
                }
            }
        }
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
            match execute_plan_day(&state, &dek, &plan, trade_date, None, None).await {
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
        warnings,
        message,
    }))
}

pub async fn smart_preview(
    State(state): State<AppState>,
    Query(query): Query<SmartPreviewQuery>,
) -> HandlerResult<Json<SmartPreviewResponse>> {
    if crate::market_data::index_option(query.index_code.trim()).is_none() {
        return Err(bad_request("请选择支持的跟踪指数"));
    }
    if !crate::market_data::valid_moving_average(query.moving_average_days) {
        return Err(bad_request("请选择支持的均线周期"));
    }
    let trade_date = if query.trade_date.trim().is_empty() {
        china_today()
    } else {
        NaiveDate::parse_from_str(query.trade_date.trim(), "%Y-%m-%d")
            .map_err(|_| bad_request("计划执行日期格式不正确"))?
    };
    let history_start = trade_date - Duration::days(i64::from(query.moving_average_days) * 2 + 60);
    let history_end = trade_date
        .pred_opt()
        .ok_or_else(|| bad_request("指数行情结束日期超出范围"))?;
    let refresh_error = crate::market_data::refresh_index(
        &state,
        query.index_code.trim(),
        history_start,
        history_end,
    )
    .await
    .err();
    let decision = crate::market_data::smart_decision(
        &state,
        query.index_code.trim(),
        query.moving_average_days,
        trade_date,
    )
    .await
    .map_err(|cache_error| {
        bad_request(match refresh_error.as_ref() {
            Some(error) => format!("{error}；{cache_error}"),
            None => cache_error,
        })
    })?;
    let warning = refresh_error
        .map(|error| format!("行情更新失败，当前使用本地缓存：{error}"))
        .unwrap_or_default();
    Ok(Json(SmartPreviewResponse {
        ok: true,
        index_name: decision.index_name,
        moving_average_days: decision.moving_average_days,
        quote_date: decision.quote_date.format("%Y-%m-%d").to_string(),
        close: crate::market_data::format_point(decision.close),
        moving_average: crate::market_data::format_point(decision.moving_average),
        deviation: crate::market_data::format_percent(decision.deviation_percent),
        multiplier: crate::market_data::format_multiplier(decision.multiplier_bps),
        source: decision.source,
        warning,
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
