use askama::Template;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{Html, Redirect},
};
use chrono::{Duration, NaiveDate};
use sea_orm::{EntityTrait, QueryOrder};
use std::collections::HashMap;

use crate::{
    credit_statements::{self, Ledger},
    crypto, currency,
    entity::{
        account, account_detail, balance_adjustment, bill, debt_record, installment_item,
        installment_plan, subscription, transfer,
    },
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

struct FundPreview {
    name: String,
    value: String,
    profit: String,
    profit_percent: String,
    has_profit: bool,
    positive: bool,
    negative: bool,
}

struct AccountSummary {
    name: String,
    kind: String,
    balance: String,
    is_investment: bool,
    funds: Vec<FundPreview>,
    fund_count: usize,
    profit: String,
    profit_percent: String,
    has_profit: bool,
    positive: bool,
    negative: bool,
    excluded_fund_count: usize,
}

struct AccountGroup {
    label: String,
    count: usize,
    subtotal: String,
    is_investment: bool,
    accounts: Vec<AccountSummary>,
}

struct InvestmentSummary {
    funds: Vec<FundPreview>,
    profit: String,
    profit_percent: String,
    has_profit: bool,
    positive: bool,
    negative: bool,
    excluded_fund_count: usize,
}

const ACCOUNT_GROUP_ORDER: &[&str] = &[
    "payment",
    "bank",
    "stored_value",
    "investment",
    "other",
    "credit_card",
    "credit_service",
];

fn account_group_key(kind: &str) -> &'static str {
    match kind {
        "payment" => "payment",
        "bank" => "bank",
        "stored_value" => "stored_value",
        "investment" => "investment",
        "credit_card" => "credit_card",
        "credit_service" => "credit_service",
        _ => "other",
    }
}

fn account_group_label(kind: &str) -> &'static str {
    match kind {
        "payment" => "支付账户",
        "bank" => "银行账户",
        "stored_value" => "储值卡账户",
        "investment" => "投资分类",
        "credit_card" => "信用卡",
        "credit_service" => "信贷服务",
        _ => "其他账户",
    }
}

fn add_net_invested(
    net_invested: &mut HashMap<i64, i64>,
    account_id: i64,
    delta: i64,
) -> HandlerResult<()> {
    let current = net_invested.get(&account_id).copied().unwrap_or_default();
    net_invested.insert(
        account_id,
        current
            .checked_add(delta)
            .ok_or_else(|| err500("投入本金超出范围"))?,
    );
    Ok(())
}

fn push_account_summary(
    grouped: &mut HashMap<&'static str, Vec<AccountSummary>>,
    subtotals: &mut HashMap<&'static str, i64>,
    kind: &str,
    summary: AccountSummary,
    converted_balance: i64,
) -> HandlerResult<()> {
    let key = account_group_key(kind);
    grouped.entry(key).or_default().push(summary);
    let subtotal = subtotals.entry(key).or_insert(0);
    *subtotal = subtotal
        .checked_add(converted_balance)
        .ok_or_else(|| err500("账户小计超出范围"))?;
    Ok(())
}

fn signed_money(cents: i64, currency: &str) -> HandlerResult<String> {
    if cents > 0 {
        Ok(format!("+{}", currency::format(cents, currency)))
    } else if cents < 0 {
        let absolute = cents.checked_neg().ok_or_else(|| err500("金额超出范围"))?;
        Ok(format!("-{}", currency::format(absolute, currency)))
    } else {
        Ok(currency::format(0, currency))
    }
}

fn profit_percent(profit: i64, invested: i64) -> String {
    if invested <= 0 {
        return String::new();
    }
    let percent = (rust_decimal::Decimal::from(profit) * rust_decimal::Decimal::from(100)
        / rust_decimal::Decimal::from(invested))
    .round_dp(1);
    let text = percent.normalize().to_string();
    if percent > rust_decimal::Decimal::ZERO {
        format!("+{text}%")
    } else {
        format!("{text}%")
    }
}

fn profit_texts(
    has_profit: bool,
    profit: i64,
    invested: i64,
    currency: &str,
) -> HandlerResult<(String, String, bool, bool)> {
    if !has_profit {
        return Ok(("—".into(), String::new(), false, false));
    }
    Ok((
        signed_money(profit, currency)?,
        profit_percent(profit, invested),
        profit > 0,
        profit < 0,
    ))
}

/// P/L preview: current fund value minus cumulative net transfer principal.
/// Funds without net principal keep "—" instead of inventing a cost basis.
fn investment_summary(
    dek: &crypto::Dek,
    root: &account::Model,
    funds: &[&account::Model],
    details: &HashMap<i64, account_detail::Model>,
    balances: &HashMap<i64, i64>,
    net_invested: &HashMap<i64, i64>,
) -> HandlerResult<InvestmentSummary> {
    let mut rows = Vec::with_capacity(funds.len());
    let mut basis_total = 0i64;
    let mut value_with_basis = 0i64;
    let mut excluded_fund_count = 0usize;
    for fund in funds {
        let value = *balances
            .get(&fund.id)
            .ok_or_else(|| err500("基金余额缺失"))?;
        let invested = net_invested.get(&fund.id).copied().unwrap_or_default();
        let has_profit = invested > 0;
        let profit = if has_profit {
            basis_total = basis_total
                .checked_add(invested)
                .ok_or_else(|| err500("投入本金超出范围"))?;
            value_with_basis = value_with_basis
                .checked_add(value)
                .ok_or_else(|| err500("持仓价值超出范围"))?;
            value
                .checked_sub(invested)
                .ok_or_else(|| err500("盈亏超出范围"))?
        } else {
            if value != 0 {
                excluded_fund_count += 1;
            }
            0
        };
        let (text, percent, positive, negative) =
            profit_texts(has_profit, profit, invested, &root.currency)?;
        rows.push(FundPreview {
            name: super::bills::account_display_name(dek, fund, details.get(&fund.id)),
            value: currency::format(value, &root.currency),
            profit: text,
            profit_percent: percent,
            has_profit,
            positive,
            negative,
        });
    }
    let has_profit = basis_total > 0;
    let profit = if has_profit {
        value_with_basis
            .checked_sub(basis_total)
            .ok_or_else(|| err500("盈亏超出范围"))?
    } else {
        0
    };
    let (text, percent, positive, negative) =
        profit_texts(has_profit, profit, basis_total, &root.currency)?;
    Ok(InvestmentSummary {
        funds: rows,
        profit: text,
        profit_percent: percent,
        has_profit,
        positive,
        negative,
        excluded_fund_count,
    })
}

struct CreditDueSummary {
    due_date: String,
    ordinary: String,
    installment: String,
    total: String,
    overdue: bool,
}

struct CreditAccountSummary {
    name: String,
    kind: String,
    used: String,
    available: String,
    limit: String,
    unbilled: String,
    due: Vec<CreditDueSummary>,
    due_total: String,
    installment_principal: String,
    next_installment: Option<CreditDueSummary>,
    has_adjustments: bool,
    has_future_entries: bool,
    usage_percent: i64,
    danger: bool,
}

struct ReminderRow {
    title: String,
    detail: String,
    due_at: String,
    amount: String,
    url: String,
    overdue: bool,
}

struct AutoDebitWarning {
    title: String,
    detail: String,
    amount: String,
    due_at: String,
    overdue: bool,
}

struct FundValuationReminder {
    parent_id: i64,
    name: String,
    parent_name: String,
    amount: String,
    days_since: i64,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    account_groups: Vec<AccountGroup>,
    credit_accounts: Vec<CreditAccountSummary>,
    auto_debit_warnings: Vec<AutoDebitWarning>,
    net_assets: String,
    total_income: String,
    total_expense: String,
    receivable: String,
    payable: String,
    subscription_reminders: Vec<ReminderRow>,
    installment_reminders: Vec<ReminderRow>,
    subscription_reminder_count: usize,
    installment_reminder_count: usize,
    budget_statuses: Vec<super::budgets::BudgetStatus>,
    valuation_reminders: Vec<FundValuationReminder>,
    valuation_reminder_count: usize,
}

fn account_kind_label(kind: &str) -> &'static str {
    match kind {
        "payment" => "支付",
        "bank" => "银行",
        "stored_value" => "储值卡",
        "credit_card" => "信用卡",
        "credit_service" => "信贷服务",
        "investment" => "投资分类",
        "investment_fund" => "基金",
        _ => "其他",
    }
}

struct CreditAmounts {
    used: i64,
    available: i64,
    usage_percent: i64,
    danger: bool,
}

fn credit_amounts(balance: i64, limit: i64) -> CreditAmounts {
    let used = balance.checked_neg().unwrap_or(i64::MAX).max(0);
    let available = limit.saturating_sub(used).max(0);
    let usage_percent = if limit > 0 {
        (i128::from(used) * 100 / i128::from(limit)).min(100) as i64
    } else if used > 0 {
        100
    } else {
        0
    };
    CreditAmounts {
        used,
        available,
        usage_percent,
        danger: used > 0 && (available == 0 || (limit > 0 && available <= limit / 5)),
    }
}

fn credit_due_summary(
    amount: credit_statements::DueAmount,
    account_currency: &str,
    today: NaiveDate,
) -> CreditDueSummary {
    CreditDueSummary {
        due_date: amount.due_date.format("%Y年%m月%d日").to_string(),
        ordinary: currency::format(amount.ordinary, account_currency),
        installment: currency::format(amount.installment, account_currency),
        total: currency::format(amount.total, account_currency),
        overdue: amount.due_date < today,
    }
}

// Reuse the account balance rules against the same snapshot as the estimates.
fn snapshot_balances(
    accounts: &[account::Model],
    ledger: &Ledger<'_>,
    dek: &crypto::Dek,
) -> HandlerResult<HashMap<i64, i64>> {
    let mut balances = HashMap::with_capacity(accounts.len());
    for account in accounts
        .iter()
        .filter(|account| crate::investment_funds::is_money_account(account))
    {
        let balance = super::accounts::transaction_balance_from_records(
            dek,
            account.id,
            ledger.bills,
            ledger.transfers,
            ledger.debts,
        )?
        .checked_add(crypto::decrypt_cents(dek, &account.balance_offset))
        .ok_or_else(|| err500("余额超出范围"))?;
        balances.insert(account.id, balance);
    }
    crate::investment_funds::roll_up_balances(accounts, &mut balances)?;
    Ok(balances)
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
) -> HandlerResult<Html<String>> {
    let today = time_zone.today();
    let balance_guard = state.balance_writes.lock().await;
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let account_currencies: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let bills = bill::Entity::find().all(&state.db).await.map_err(err500)?;
    let refunds = super::refunds::RefundTotals::new(&dek, &bills);
    let transfers = transfer::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let debt_records = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let adjustments = balance_adjustment::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let reminder_plans: HashMap<i64, installment_plan::Model> = installment_plan::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|plan| plan.deleted_at.is_none())
        .map(|plan| (plan.id, plan))
        .collect();
    let installment_items = installment_item::Entity::find()
        .order_by_asc(installment_item::Column::DueDate)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let ledger = Ledger {
        bills: &bills,
        transfers: &transfers,
        debts: &debt_records,
        adjustments: &adjustments,
        plans: &reminder_plans,
        items: &installment_items,
    };
    let account_balances = snapshot_balances(&accounts, &ledger, &dek)?;
    let mut statements = HashMap::new();
    for account in accounts.iter().filter(|account| {
        account.parent_id.is_none()
            && matches!(account.kind.as_str(), "credit_card" | "credit_service")
            && account_balances.get(&account.id) != Some(&0)
    }) {
        let detail = details.get(&account.id);
        let limit = detail
            .map(|detail| crypto::decrypt_cents(&dek, &detail.credit_limit))
            .unwrap_or_default();
        let statement = if account.kind == "credit_card" && limit == 0 {
            None
        } else {
            Some(
                credit_statements::calculate(account, detail, &ledger, &dek, time_zone, today)
                    .map_err(err500)?,
            )
        };
        statements.insert(account.id, (limit, statement));
    }
    drop(balance_guard);
    let default_currency = currency::default_currency(&state).await.map_err(err500)?;
    let currencies = accounts
        .iter()
        .map(|account| account.currency.clone())
        .collect::<Vec<_>>();
    let rates = currency::RateTable::load(&state, currencies, &default_currency, today)
        .await
        .map_err(err500)?;
    let reminder_bills: HashMap<i64, &bill::Model> =
        bills.iter().map(|bill| (bill.id, bill)).collect();
    let account_names = super::bills::account_display_names(&dek, &accounts, &details);
    let mut funds_by_parent: HashMap<i64, Vec<&account::Model>> = HashMap::new();
    for fund in accounts
        .iter()
        .filter(|account| account.kind == "investment_fund")
    {
        let parent = fund.parent_id.ok_or_else(|| err500("基金缺少投资分组"))?;
        funds_by_parent.entry(parent).or_default().push(fund);
    }
    let mut net_invested: HashMap<i64, i64> = HashMap::new();
    for transfer in &transfers {
        let outgoing = crypto::decrypt_cents(&dek, &transfer.amount);
        let incoming = super::transfer_to_cents(&dek, transfer);
        add_net_invested(
            &mut net_invested,
            transfer.from_account_id,
            outgoing
                .checked_neg()
                .ok_or_else(|| err500("投入本金超出范围"))?,
        )?;
        add_net_invested(&mut net_invested, transfer.to_account_id, incoming)?;
    }
    let due_valuations = crate::investment_funds::valuation_reminders(
        &dek,
        &accounts,
        &account_balances,
        chrono::Utc::now(),
    )?;
    let valuation_reminder_count = due_valuations.len();
    let valuation_reminders = due_valuations
        .into_iter()
        .map(|fund| FundValuationReminder {
            parent_id: fund.parent_id,
            name: fund.name,
            parent_name: fund.parent_name,
            amount: currency::format(fund.amount, &fund.currency),
            days_since: fund.days_since,
        })
        .collect();
    let mut net_assets = 0i64;
    let mut grouped_accounts: HashMap<&'static str, Vec<AccountSummary>> = HashMap::new();
    let mut group_subtotals: HashMap<&'static str, i64> = HashMap::new();
    let mut credit_accounts = Vec::new();
    for account in accounts
        .iter()
        .filter(|account| account.parent_id.is_none())
    {
        let balance = *account_balances
            .get(&account.id)
            .ok_or_else(|| err500("账户余额缺失"))?;
        let converted = rates.convert(balance, &account.currency).map_err(err500)?;
        net_assets = net_assets
            .checked_add(converted)
            .ok_or_else(|| err500("资产金额超出范围"))?;
        let name = account_names.get(&account.id).cloned().unwrap_or_default();
        if matches!(account.kind.as_str(), "credit_card" | "credit_service") {
            if balance == 0 {
                continue;
            }
            let (limit, statement) = statements
                .remove(&account.id)
                .ok_or_else(|| err500("信用账户账单估算缺失"))?;
            if account.kind == "credit_card" && limit == 0 {
                push_account_summary(
                    &mut grouped_accounts,
                    &mut group_subtotals,
                    &account.kind,
                    AccountSummary {
                        name,
                        kind: account_kind_label(&account.kind).into(),
                        balance: currency::format(balance, &account.currency),
                        is_investment: false,
                        funds: Vec::new(),
                        fund_count: 0,
                        profit: String::new(),
                        profit_percent: String::new(),
                        has_profit: false,
                        positive: false,
                        negative: false,
                        excluded_fund_count: 0,
                    },
                    converted,
                )?;
                continue;
            }
            let statement = statement.ok_or_else(|| err500("信用账户账单估算缺失"))?;
            let amounts = credit_amounts(balance, limit);
            let due_total = statement.due.iter().try_fold(0i64, |total, due| {
                total
                    .checked_add(due.total)
                    .ok_or_else(|| err500("已出账应还金额超出范围"))
            })?;
            credit_accounts.push(CreditAccountSummary {
                name,
                kind: account_kind_label(&account.kind).into(),
                used: currency::format(amounts.used, &account.currency),
                available: currency::format(amounts.available, &account.currency),
                limit: currency::format(limit, &account.currency),
                unbilled: currency::format(statement.unbilled, &account.currency),
                due_total: currency::format(due_total, &account.currency),
                due: statement
                    .due
                    .into_iter()
                    .map(|due| credit_due_summary(due, &account.currency, today))
                    .collect(),
                installment_principal: currency::format(
                    statement.installment_principal,
                    &account.currency,
                ),
                next_installment: statement
                    .next_installment
                    .map(|due| credit_due_summary(due, &account.currency, today)),
                has_adjustments: statement.has_adjustments,
                has_future_entries: statement.has_future_entries,
                usage_percent: amounts.usage_percent,
                danger: amounts.danger,
            });
        } else if account.kind == "investment" {
            let summary = investment_summary(
                &dek,
                account,
                funds_by_parent
                    .get(&account.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                &details,
                &account_balances,
                &net_invested,
            )?;
            let fund_count = summary.funds.len();
            push_account_summary(
                &mut grouped_accounts,
                &mut group_subtotals,
                &account.kind,
                AccountSummary {
                    name,
                    kind: account_kind_label(&account.kind).into(),
                    balance: currency::format(balance, &account.currency),
                    is_investment: true,
                    funds: summary.funds,
                    fund_count,
                    profit: summary.profit,
                    profit_percent: summary.profit_percent,
                    has_profit: summary.has_profit,
                    positive: summary.positive,
                    negative: summary.negative,
                    excluded_fund_count: summary.excluded_fund_count,
                },
                converted,
            )?;
        } else {
            push_account_summary(
                &mut grouped_accounts,
                &mut group_subtotals,
                &account.kind,
                AccountSummary {
                    name,
                    kind: account_kind_label(&account.kind).into(),
                    balance: currency::format(balance, &account.currency),
                    is_investment: false,
                    funds: Vec::new(),
                    fund_count: 0,
                    profit: String::new(),
                    profit_percent: String::new(),
                    has_profit: false,
                    positive: false,
                    negative: false,
                    excluded_fund_count: 0,
                },
                converted,
            )?;
        }
    }
    let account_groups = ACCOUNT_GROUP_ORDER
        .iter()
        .filter_map(|key| {
            let accounts = grouped_accounts.remove(key)?;
            Some(AccountGroup {
                label: account_group_label(key).into(),
                count: accounts.len(),
                subtotal: currency::format(
                    group_subtotals.get(key).copied().unwrap_or_default(),
                    &default_currency,
                ),
                is_investment: *key == "investment",
                accounts,
            })
        })
        .collect::<Vec<_>>();
    let now = chrono::Utc::now().naive_utc();
    let reminder_deadline = now + Duration::days(7);
    let subscriptions = subscription::Entity::find()
        .order_by_asc(subscription::Column::ExpiresAt)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut all_subscription_reminders = Vec::new();
    let mut auto_debit_warnings = Vec::new();
    for item in &subscriptions {
        let name = crypto::decrypt_string(&dek, &item.name);
        let amount = crypto::decrypt_cents(&dek, &item.amount);
        if item.expires_at <= reminder_deadline {
            let auto_detail = item
                .auto_debit_account_id
                .and_then(|id| account_names.get(&id))
                .map(|name| format!(" · 自动扣款：{name}"))
                .unwrap_or_default();
            all_subscription_reminders.push(ReminderRow {
                title: name.clone(),
                detail: format!(
                    "{} · {}{auto_detail}",
                    crypto::decrypt_string(&dek, &item.category),
                    item.currency
                ),
                due_at: item.expires_at.format(TIME_FMT).to_string(),
                amount: currency::format(amount, &item.currency),
                url: "/subscriptions".into(),
                overdue: item.expires_at < now,
            });
        }
        let Some(account_id) = item.auto_debit_account_id else {
            continue;
        };
        let check_days = item.balance_check_days.clamp(0, 30);
        if now < item.expires_at - Duration::days(i64::from(check_days)) {
            continue;
        }
        let warning = match accounts.iter().find(|account| account.id == account_id) {
            None => Some("自动扣款账户已不存在，请重新配置".to_string()),
            Some(account) if account.currency != item.currency => {
                Some("自动扣款账户与订阅货币不一致，请重新配置".to_string())
            }
            Some(account) => {
                let balance = account_balances
                    .get(&account.id)
                    .copied()
                    .unwrap_or_default();
                let available = if matches!(account.kind.as_str(), "credit_card" | "credit_service")
                {
                    let limit = details
                        .get(&account.id)
                        .map(|detail| crypto::decrypt_cents(&dek, &detail.credit_limit))
                        .unwrap_or_default();
                    balance.saturating_add(limit)
                } else {
                    balance
                };
                (available < amount).then(|| {
                    let label = if matches!(account.kind.as_str(), "credit_card" | "credit_service")
                    {
                        "可用额度"
                    } else {
                        "余额"
                    };
                    format!(
                        "{}的{label}仅有 {}，不足以支付 {}",
                        account_names.get(&account.id).cloned().unwrap_or_default(),
                        currency::format(available, &account.currency),
                        currency::format(amount, &item.currency)
                    )
                })
            }
        };
        if let Some(detail) = warning {
            auto_debit_warnings.push(AutoDebitWarning {
                title: name,
                detail,
                amount: currency::format(amount, &item.currency),
                due_at: item.expires_at.format(TIME_FMT).to_string(),
                overdue: item.expires_at < now,
            });
        }
    }
    let subscription_reminder_count = all_subscription_reminders.len();
    let subscription_reminders = all_subscription_reminders.into_iter().take(10).collect();

    let mut all_installment_reminders = Vec::new();
    let refund_context =
        crate::installment_state::RefundContext::new(&dek, &bills).map_err(err500)?;
    for plan in reminder_plans.values() {
        let refund = refund_context.state(&dek, plan).map_err(err500)?;
        if refund.needs_review {
            all_installment_reminders.push(ReminderRow {
                title: "退款后分期待核对".into(),
                detail: "已暂停还款，请按银行剩余计划逐期确认".into(),
                due_at: "需要优先处理".into(),
                amount: String::new(),
                url: format!("/installments/{}", plan.id),
                overdue: true,
            });
        }
    }
    for item in installment_items
        .iter()
        .filter(|item| item.paid_at.is_none() && item.due_date <= today + Duration::days(7))
    {
        let Some(plan) = reminder_plans.get(&item.plan_id) else {
            continue;
        };
        let refund = refund_context.state(&dek, plan).map_err(err500)?;
        if refund.full_refund || refund.needs_review {
            continue;
        }
        let plan_bill = plan.bill_id.and_then(|id| reminder_bills.get(&id));
        let Some(plan_currency) = account_currencies.get(&plan.account_id) else {
            continue;
        };
        let title = match plan_bill {
            None => "最低还款余款分期".into(),
            Some(bill) if bill.is_private => "隐私账单".into(),
            Some(bill) => {
                let category = crypto::decrypt_string(&dek, &bill.category);
                let note = crypto::decrypt_string(&dek, &bill.note);
                if note.is_empty() {
                    category
                } else {
                    format!("{category} · {note}")
                }
            }
        };
        all_installment_reminders.push(ReminderRow {
            title,
            detail: format!(
                "第 {} 期 · {}",
                item.sequence,
                account_names
                    .get(&plan.account_id)
                    .cloned()
                    .unwrap_or_default()
            ),
            due_at: item.due_date.format("%Y-%m-%d").to_string(),
            amount: currency::format(crypto::decrypt_cents(&dek, &item.total), plan_currency),
            url: format!("/installments/{}", plan.id),
            overdue: item.due_date < today,
        });
    }
    let installment_reminder_count = all_installment_reminders.len();
    let installment_reminders = all_installment_reminders.into_iter().take(10).collect();
    let mut receivable = 0i64;
    let mut payable = 0i64;
    for (remaining_receivable, remaining_payable) in super::debts::outstanding_balances(
        &state,
        &dek,
        &debt_records,
        &account_currencies,
        &rates,
        &default_currency,
    )
    .await?
    .into_values()
    {
        receivable = receivable
            .checked_add(remaining_receivable)
            .ok_or_else(|| err500("借贷金额超出范围"))?;
        payable = payable
            .checked_add(remaining_payable)
            .ok_or_else(|| err500("借贷金额超出范围"))?;
    }

    let mut total_income = 0i64;
    let mut total_expense = 0i64;
    for bill in &bills {
        let native_amount = refunds.amount(&dek, bill);
        if native_amount == 0 {
            continue;
        }
        let bill_currency = account_currencies
            .get(&bill.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let amount = rates
            .convert(native_amount, bill_currency)
            .map_err(err500)?;
        let target = if bill.kind == "income" {
            &mut total_income
        } else {
            &mut total_expense
        };
        *target = target
            .checked_add(amount)
            .ok_or_else(|| err500("报表金额超出范围"))?;
    }
    let budget_statuses = super::budgets::current_statuses(&state, &dek, time_zone).await?;
    let html = DashboardTemplate {
        account_groups,
        credit_accounts,
        auto_debit_warnings,
        net_assets: currency::format(net_assets, &default_currency),
        total_income: currency::format(total_income, &default_currency),
        total_expense: currency::format(total_expense, &default_currency),
        receivable: currency::format(receivable, &default_currency),
        payable: currency::format(payable, &default_currency),
        subscription_reminders,
        installment_reminders,
        subscription_reminder_count,
        installment_reminder_count,
        valuation_reminders,
        valuation_reminder_count,
        budget_statuses,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn redirect() -> Redirect {
    Redirect::to("/dashboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_debt_and_positive_balances_never_warn() {
        for limit in [0, 100_000] {
            for balance in [0, 10_000] {
                let amounts = credit_amounts(balance, limit);
                assert_eq!(amounts.used, 0);
                assert_eq!(amounts.usage_percent, 0);
                assert!(!amounts.danger);
            }
        }
    }

    #[test]
    fn high_usage_warns_without_changing_used_or_available_amounts() {
        let amounts = credit_amounts(-90_000, 100_000);
        assert_eq!(amounts.used, 90_000);
        assert_eq!(amounts.available, 10_000);
        assert_eq!(amounts.usage_percent, 90);
        assert!(amounts.danger);
        let comfortable = credit_amounts(-50_000, 100_000);
        assert_eq!(comfortable.available, 50_000);
        assert!(!comfortable.danger);
    }

    #[test]
    fn debt_without_a_limit_or_above_limit_warns() {
        for limit in [0, 50_000] {
            let amounts = credit_amounts(-60_000, limit);
            assert_eq!(amounts.used, 60_000);
            assert_eq!(amounts.available, 0);
            assert_eq!(amounts.usage_percent, 100);
            assert!(amounts.danger);
        }
    }

    fn model(dek: &crypto::Dek, id: i64, kind: &str, parent_id: Option<i64>) -> account::Model {
        account::Model {
            id,
            name: crypto::encrypt(dek, format!("账户{id}").as_bytes()),
            kind: kind.into(),
            currency: "CNY".into(),
            balance_offset: crypto::encrypt_cents(dek, 0),
            note: crypto::encrypt(dek, b""),
            created_at: chrono::Utc::now(),
            parent_id,
            last_calibrated_at: None,
            sms_names: crypto::encrypt(dek, b"[]"),
            transfer_fee_rate_bps: crypto::encrypt_cents(dek, 0),
        }
    }

    #[test]
    fn signed_money_and_percent_keep_direction_and_rounding() {
        assert_eq!(signed_money(12_345, "CNY").unwrap(), "+CNY 123.45");
        assert_eq!(signed_money(-12_345, "CNY").unwrap(), "-CNY 123.45");
        assert_eq!(signed_money(0, "CNY").unwrap(), "CNY 0.00");
        assert_eq!(profit_percent(20_000, 100_000), "+20%");
        assert_eq!(profit_percent(-5_000, 100_000), "-5%");
        assert_eq!(profit_percent(0, 100_000), "0%");
        assert_eq!(profit_percent(1, 3), "+33.3%");
        assert_eq!(profit_percent(1_000, 0), "");
    }

    #[test]
    fn investment_summary_uses_net_transfer_principal_and_skips_unknown_cost() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let root = model(&dek, 1, "investment", None);
        let invested_fund = model(&dek, 2, "investment_fund", Some(1));
        let initial_fund = model(&dek, 3, "investment_fund", Some(1));
        let details = HashMap::new();

        let summary = investment_summary(
            &dek,
            &root,
            &[&invested_fund],
            &details,
            &HashMap::from([(2, 120_000)]),
            &HashMap::from([(2, 100_000)]),
        )
        .unwrap();
        assert!(summary.has_profit);
        assert_eq!(summary.profit, "+CNY 200.00");
        assert_eq!(summary.profit_percent, "+20%");
        assert_eq!(summary.excluded_fund_count, 0);

        let loss = investment_summary(
            &dek,
            &root,
            &[&invested_fund],
            &details,
            &HashMap::from([(2, 90_000)]),
            &HashMap::from([(2, 100_000)]),
        )
        .unwrap();
        assert!(loss.has_profit);
        assert!(loss.negative);
        assert_eq!(loss.profit, "-CNY 100.00");
        assert_eq!(loss.profit_percent, "-10%");

        let mixed = investment_summary(
            &dek,
            &root,
            &[&invested_fund, &initial_fund],
            &details,
            &HashMap::from([(2, 120_000), (3, 50_000)]),
            &HashMap::from([(2, 100_000)]),
        )
        .unwrap();
        assert!(mixed.has_profit);
        assert_eq!(mixed.profit, "+CNY 200.00");
        assert_eq!(mixed.excluded_fund_count, 1);
        assert!(!mixed.funds[1].has_profit);
        assert_eq!(mixed.funds[1].profit, "—");

        let unknown = investment_summary(
            &dek,
            &root,
            &[&initial_fund],
            &details,
            &HashMap::from([(3, 50_000)]),
            &HashMap::new(),
        )
        .unwrap();
        assert!(!unknown.has_profit);
        assert_eq!(unknown.profit, "—");
    }
}
