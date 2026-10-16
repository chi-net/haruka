use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use chrono::{Datelike, NaiveDate, NaiveDateTime, Utc};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde::Deserialize;

use crate::{
    crypto, currency,
    entity::{
        account, account_detail, balance_adjustment, bill, debt_person, debt_record, transfer,
    },
    AppState, SessionDek,
};

use super::ClientTimeZone;

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

fn add(left: i64, right: i64) -> HandlerResult<i64> {
    left.checked_add(right)
        .ok_or_else(|| err500("对账金额超出范围"))
}

fn subtract(left: i64, right: i64) -> HandlerResult<i64> {
    left.checked_sub(right)
        .ok_or_else(|| err500("对账金额超出范围"))
}

// A corrupt encrypted amount must not silently become a zero in a statement.
fn read_cents(dek: &crypto::Dek, value: &str) -> HandlerResult<i64> {
    crypto::decrypt_string(dek, value)
        .parse()
        .map_err(|_| err500("账本金额无法读取"))
}

fn signed_amount(dek: &crypto::Dek, value: &str, incoming: bool) -> HandlerResult<i64> {
    let cents = read_cents(dek, value)?;
    if cents < 0 {
        return Err(err500("账本流水金额不能为负数"));
    }
    if incoming {
        Ok(cents)
    } else {
        subtract(0, cents)
    }
}

#[derive(Default, Deserialize)]
pub struct StatementQuery {
    #[serde(default)]
    start: String,
    #[serde(default)]
    end: String,
    #[serde(default)]
    time_zone: String,
}

#[derive(Deserialize)]
pub struct StatementCompareForm {
    #[serde(flatten)]
    period: StatementQuery,
    actual_balance: String,
}

fn parse_actual_balance(value: &str) -> HandlerResult<i64> {
    let value =
        Decimal::from_str(value.trim()).map_err(|_| bad_request("实际期末余额格式不正确"))?;
    let cents = value
        .checked_mul(Decimal::from(100))
        .ok_or_else(|| bad_request("实际期末余额超出范围"))?;
    if cents.fract() != Decimal::ZERO {
        return Err(bad_request("实际期末余额最多支持两位小数"));
    }
    cents
        .to_i64()
        .ok_or_else(|| bad_request("实际期末余额超出范围"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlowKind {
    Income,
    Expense,
    TransferIn,
    TransferOut,
    DebtIn,
    DebtOut,
    Adjustment,
}

impl FlowKind {
    fn index(self) -> usize {
        self as usize
    }
}

struct LedgerEvent {
    happened_at: NaiveDateTime,
    created_at: chrono::DateTime<Utc>,
    source: u8,
    id: i64,
    kind: FlowKind,
    delta: i64,
    label: &'static str,
    subject: String,
    note: String,
}

struct StatementBalances {
    opening: i64,
    closing: i64,
    current: i64,
    net: i64,
    totals: [i64; 7],
    rows: Vec<(usize, i64)>,
}

fn calculate_balances(
    offset: i64,
    events: &[LedgerEvent],
    zone: ClientTimeZone,
    start: NaiveDate,
    end: NaiveDate,
) -> HandlerResult<StatementBalances> {
    let mut adjustments = 0;
    for event in events
        .iter()
        .filter(|event| event.kind == FlowKind::Adjustment)
    {
        adjustments = add(adjustments, event.delta)?;
    }
    // The offset already contains all calibration changes. Remove them once,
    // then replay each audit delta at its historical occurrence.
    let baseline = subtract(offset, adjustments)?;
    let mut result = StatementBalances {
        opening: baseline,
        closing: baseline,
        current: baseline,
        net: 0,
        totals: [0; 7],
        rows: Vec::new(),
    };
    for (index, event) in events.iter().enumerate() {
        result.current = add(result.current, event.delta)?;
        let date = zone.date(event.happened_at);
        if date < start {
            result.opening = add(result.opening, event.delta)?;
        }
        if date <= end {
            result.closing = add(result.closing, event.delta)?;
        }
        if date >= start && date <= end {
            result.net = add(result.net, event.delta)?;
            let total = &mut result.totals[event.kind.index()];
            *total = add(*total, event.delta)?;
            result.rows.push((index, result.current));
        }
    }
    if add(result.opening, result.net)? != result.closing {
        return Err(err500("对账期初、净变动与期末余额不一致"));
    }
    Ok(result)
}

struct StatementRow {
    number: usize,
    happened_at: String,
    reference: String,
    label: &'static str,
    subject: String,
    note: String,
    amount: String,
    balance: String,
}

struct SummaryRow {
    label: &'static str,
    amount: String,
}

#[derive(Template)]
#[template(path = "account_statement.html")]
struct StatementTemplate {
    id: i64,
    name: String,
    currency: String,
    start: String,
    end: String,
    time_zone: String,
    generated_at: String,
    opening: String,
    closing: String,
    net: String,
    current: String,
    ending_is_current: bool,
    summaries: Vec<SummaryRow>,
    rows: Vec<StatementRow>,
    compared: bool,
    matches: bool,
    actual: String,
    difference: String,
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(zone): Extension<ClientTimeZone>,
    Path(id): Path<i64>,
    Query(query): Query<StatementQuery>,
) -> HandlerResult<Response> {
    render_statement(&state, &dek, zone, id, query, None).await
}

/// Read-only POST: the actual balance is never placed in a URL or persisted.
pub async fn compare(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(zone): Extension<ClientTimeZone>,
    Path(id): Path<i64>,
    Form(form): Form<StatementCompareForm>,
) -> HandlerResult<Response> {
    let actual = parse_actual_balance(&form.actual_balance)?;
    render_statement(&state, &dek, zone, id, form.period, Some(actual)).await
}

async fn render_statement(
    state: &AppState,
    dek: &crypto::Dek,
    zone: ClientTimeZone,
    id: i64,
    query: StatementQuery,
    actual: Option<i64>,
) -> HandlerResult<Response> {
    let zone = zone.from_query(&query.time_zone)?;
    let today = zone.today();
    let start = super::parse_search_date(&query.start, "开始日期")?
        .unwrap_or_else(|| today.with_day(1).expect("自然月首日有效"));
    let end = super::parse_search_date(&query.end, "结束日期")?.unwrap_or(today);
    if start > end {
        return Err(bad_request("开始日期不能晚于结束日期"));
    }

    let _balance_guard = state.balance_writes.lock().await;
    let target = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账户不存在".into()))?;
    if target.kind == "investment" {
        return Ok(Redirect::to(&format!("/accounts/{id}/funds")).into_response());
    }
    let bills = bill::Entity::find()
        .filter(bill::Column::AccountId.eq(id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let transfers = transfer::Entity::find()
        .filter(
            Condition::any()
                .add(transfer::Column::FromAccountId.eq(id))
                .add(transfer::Column::ToAccountId.eq(id)),
        )
        .all(&state.db)
        .await
        .map_err(err500)?;
    let debts = debt_record::Entity::find()
        .filter(debt_record::Column::AccountId.eq(id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let adjustments = balance_adjustment::Entity::find()
        .filter(balance_adjustment::Column::AccountId.eq(id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let accounts = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let details: HashMap<_, _> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|item| (item.account_id, item))
        .collect();
    let names = super::bills::account_display_names(dek, &accounts, &details);
    let people: HashMap<_, _> = debt_person::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|person| (person.id, crypto::decrypt_string(dek, &person.name)))
        .collect();
    let generated_at = Utc::now()
        .with_timezone(&zone.0)
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string();
    drop(_balance_guard);

    let offset = read_cents(dek, &target.balance_offset)?;
    // Reuse the same balance definition as current_balance, on this locked snapshot.
    let expected_current = add(
        offset,
        super::accounts::transaction_balance_from_records(dek, id, &bills, &transfers, &debts)?,
    )?;
    let fee_bills: HashSet<_> = transfers
        .iter()
        .filter_map(|item| item.fee_bill_id)
        .collect();
    let mut events =
        Vec::with_capacity(bills.len() + transfers.len() + debts.len() + adjustments.len());
    for item in bills {
        let incoming = match item.kind.as_str() {
            "income" => true,
            "expense" => false,
            _ => return Err(err500("账单收支方向无效")),
        };
        let delta = signed_amount(dek, &item.amount, incoming)?;
        events.push(LedgerEvent {
            happened_at: item.happened_at,
            created_at: item.created_at,
            source: 0,
            id: item.id,
            kind: if incoming {
                FlowKind::Income
            } else {
                FlowKind::Expense
            },
            delta,
            label: if item.is_private {
                if incoming {
                    "收入"
                } else {
                    "支出"
                }
            } else if item.refund_of_bill_id.is_some() && incoming {
                "收入 · 退款"
            } else if fee_bills.contains(&item.id) && !incoming {
                "支出 · 自动转账手续费"
            } else if incoming {
                "收入"
            } else {
                "支出"
            },
            subject: if item.is_private {
                "隐私账单".into()
            } else {
                crypto::decrypt_string(dek, &item.category)
            },
            note: if item.is_private {
                String::new()
            } else {
                crypto::decrypt_string(dek, &item.note)
            },
        });
    }
    for item in transfers {
        let incoming = item.to_account_id == id;
        let value = if incoming && !item.to_amount.is_empty() {
            &item.to_amount
        } else {
            &item.amount
        };
        let delta = signed_amount(dek, value, incoming)?;
        let other_id = if incoming {
            item.from_account_id
        } else {
            item.to_account_id
        };
        let other = names
            .get(&other_id)
            .map(String::as_str)
            .unwrap_or("已删除账户");
        events.push(LedgerEvent {
            happened_at: item.happened_at,
            created_at: item.created_at,
            source: 1,
            id: item.id,
            kind: if incoming {
                FlowKind::TransferIn
            } else {
                FlowKind::TransferOut
            },
            delta,
            label: if incoming { "转入" } else { "转出" },
            subject: format!("{} {other}", if incoming { "来自" } else { "转至" }),
            note: crypto::decrypt_string(dek, &item.note),
        });
    }
    for item in debts {
        let (incoming, label) = match item.kind.as_str() {
            "borrow" => (true, "借入"),
            "repayment_received" => (true, "收回还款"),
            "lend" => (false, "借出"),
            "repayment_paid" => (false, "归还借款"),
            _ => return Err(err500("借还流水方向无效")),
        };
        let delta = signed_amount(dek, &item.amount, incoming)?;
        events.push(LedgerEvent {
            happened_at: item.happened_at,
            created_at: item.created_at,
            source: 2,
            id: item.id,
            kind: if incoming {
                FlowKind::DebtIn
            } else {
                FlowKind::DebtOut
            },
            delta,
            label,
            subject: people
                .get(&item.person_id)
                .cloned()
                .unwrap_or_else(|| "已删除对象".into()),
            note: crypto::decrypt_string(dek, &item.note),
        });
    }
    for item in adjustments {
        let from = read_cents(dek, &item.from_balance)?;
        let to = read_cents(dek, &item.to_balance)?;
        events.push(LedgerEvent {
            happened_at: item.happened_at,
            created_at: item.created_at,
            source: 3,
            id: item.id,
            kind: FlowKind::Adjustment,
            delta: subtract(to, from)?,
            label: "余额校准",
            subject: "校准差额".into(),
            note: format!(
                "{} → {}",
                currency::format(from, &target.currency),
                currency::format(to, &target.currency)
            ),
        });
    }
    events.sort_unstable_by_key(|event| {
        (event.happened_at, event.created_at, event.source, event.id)
    });
    let balances = calculate_balances(offset, &events, zone, start, end)?;
    if balances.current != expected_current {
        return Err(err500("对账重建余额与当前账本余额不一致"));
    }
    let summary_labels = [
        "账单收入（含退款）",
        "账单支出（含自动费用）",
        "转账流入",
        "转账流出",
        "借还流入",
        "借还流出",
        "校准净额",
    ];
    let summaries = summary_labels
        .into_iter()
        .zip(balances.totals)
        .map(|(label, value)| SummaryRow {
            label,
            amount: signed_format(value, &target.currency),
        })
        .collect();
    let rows = balances
        .rows
        .iter()
        .enumerate()
        .map(|(number, (index, balance))| {
            let event = &mut events[*index];
            StatementRow {
                number: number + 1,
                happened_at: zone
                    .local_datetime(event.happened_at)
                    .format("%Y-%m-%d %H:%M:%S %:z")
                    .to_string(),
                reference: format!(
                    "{}-{}",
                    ["B", "T", "D", "A"][usize::from(event.source)],
                    event.id
                ),
                label: event.label,
                subject: std::mem::take(&mut event.subject),
                note: std::mem::take(&mut event.note),
                amount: signed_format(event.delta, &target.currency),
                balance: currency::format(*balance, &target.currency),
            }
        })
        .collect();
    let difference = actual
        .map(|actual| {
            actual
                .checked_sub(balances.closing)
                .ok_or_else(|| bad_request("实际余额与账本余额的差额超出范围"))
        })
        .transpose()?;
    let html = StatementTemplate {
        id,
        name: names
            .get(&id)
            .cloned()
            .ok_or_else(|| err500("账户名称不存在"))?,
        currency: target.currency.clone(),
        start: start.to_string(),
        end: end.to_string(),
        time_zone: zone.0.name().into(),
        generated_at,
        opening: currency::format(balances.opening, &target.currency),
        closing: currency::format(balances.closing, &target.currency),
        net: signed_format(balances.net, &target.currency),
        current: currency::format(balances.current, &target.currency),
        ending_is_current: balances.closing == balances.current,
        summaries,
        rows,
        compared: actual.is_some(),
        matches: difference == Some(0),
        actual: actual
            .map(|value| currency::format(value, &target.currency))
            .unwrap_or_default(),
        difference: difference
            .map(|value| signed_format(value, &target.currency))
            .unwrap_or_default(),
    }
    .render()
    .map_err(err500)?;
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Html(html),
    )
        .into_response())
}

fn signed_format(value: i64, code: &str) -> String {
    if value > 0 {
        format!("+{}", currency::format(value, code))
    } else {
        currency::format(value, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(day: u32, kind: FlowKind, delta: i64, id: i64) -> LedgerEvent {
        let happened_at = NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        LedgerEvent {
            happened_at,
            created_at: happened_at.and_utc(),
            source: 0,
            id,
            kind,
            delta,
            label: "",
            subject: String::new(),
            note: String::new(),
        }
    }

    #[test]
    fn historical_calibrations_are_replayed_once() {
        let events = [
            event(1, FlowKind::Income, 100, 1),
            event(2, FlowKind::Adjustment, 50, 2),
            event(3, FlowKind::Expense, -20, 3),
            event(4, FlowKind::Adjustment, -10, 4),
        ];
        let result = calculate_balances(
            1040,
            &events,
            ClientTimeZone(chrono_tz::UTC),
            NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 10, 3).unwrap(),
        )
        .unwrap();
        assert_eq!(result.opening, 1100);
        assert_eq!(result.net, 30);
        assert_eq!(result.closing, 1130);
        assert_eq!(result.current, 1120);
        assert_eq!(result.rows, [(1, 1150), (2, 1130)]);
        assert_eq!(result.opening + result.net, result.closing);
    }

    #[test]
    fn empty_period_preserves_opening_balance() {
        let events = [
            event(1, FlowKind::Income, 100, 1),
            event(5, FlowKind::Expense, -20, 2),
        ];
        let result = calculate_balances(
            300,
            &events,
            ClientTimeZone(chrono_tz::UTC),
            NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(),
        )
        .unwrap();
        assert_eq!(result.opening, 400);
        assert_eq!(result.closing, 400);
        assert_eq!(result.net, 0);
        assert!(result.rows.is_empty());
    }

    #[test]
    fn overflowing_history_and_overprecise_actual_balance_are_rejected() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert!(calculate_balances(
            i64::MAX,
            &[event(1, FlowKind::Income, 1, 1)],
            ClientTimeZone(chrono_tz::UTC),
            date,
            date
        )
        .is_err());
        assert!(parse_actual_balance("1.001").is_err());
        assert!(parse_actual_balance("92233720368547758.08").is_err());
        assert_eq!(parse_actual_balance("-12.34").unwrap(), -1234);
    }
}
