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
        account, account_detail, balance_adjustment, bill, category, debt_record, installment_item,
        installment_plan, subscription, transfer,
    },
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

struct AccountSummary {
    name: String,
    kind: String,
    balance: String,
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
    balance: String,
    used: String,
    available: String,
    limit: String,
    unbilled: String,
    due: Vec<CreditDueSummary>,
    due_total: String,
    follows_billing_day: bool,
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
    account_summaries: Vec<AccountSummary>,
    credit_accounts: Vec<CreditAccountSummary>,
    auto_debit_warnings: Vec<AutoDebitWarning>,
    net_assets: String,
    total_income: String,
    total_expense: String,
    receivable: String,
    payable: String,
    time_zone: String,
    subscription_reminders: Vec<ReminderRow>,
    installment_reminders: Vec<ReminderRow>,
    subscription_reminder_count: usize,
    installment_reminder_count: usize,
    budget_statuses: Vec<super::budgets::BudgetStatus>,
    category_count_statuses: Vec<crate::category_limits::CountStatus>,
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
    let due_valuations = crate::investment_funds::valuation_reminders(
        &dek,
        &accounts,
        &account_balances,
        chrono::Utc::now(),
    )?;
    let valuation_reminder_count = due_valuations.len();
    let valuation_reminders = due_valuations
        .into_iter()
        .take(10)
        .map(|fund| FundValuationReminder {
            parent_id: fund.parent_id,
            name: fund.name,
            parent_name: fund.parent_name,
            amount: currency::format(fund.amount, &fund.currency),
            days_since: fund.days_since,
        })
        .collect();
    let mut net_assets = 0i64;
    let mut account_summaries = Vec::with_capacity(accounts.len());
    let mut credit_accounts = Vec::new();
    for account in accounts
        .iter()
        .filter(|account| account.parent_id.is_none())
    {
        let balance = *account_balances
            .get(&account.id)
            .ok_or_else(|| err500("账户余额缺失"))?;
        net_assets = net_assets
            .checked_add(rates.convert(balance, &account.currency).map_err(err500)?)
            .ok_or_else(|| err500("资产金额超出范围"))?;
        if matches!(account.kind.as_str(), "credit_card" | "credit_service") {
            if balance == 0 {
                continue;
            }
            let detail = details.get(&account.id);
            let (limit, statement) = statements
                .remove(&account.id)
                .ok_or_else(|| err500("信用账户账单估算缺失"))?;
            if account.kind == "credit_card" && limit == 0 {
                account_summaries.push(AccountSummary {
                    name: account_names.get(&account.id).cloned().unwrap_or_default(),
                    kind: account_kind_label(&account.kind).into(),
                    balance: currency::format(balance, &account.currency),
                });
                continue;
            }
            let statement = statement.ok_or_else(|| err500("信用账户账单估算缺失"))?;
            let amounts = credit_amounts(balance, limit);
            let due_total = statement.due.iter().try_fold(0i64, |total, due| {
                total
                    .checked_add(due.total)
                    .ok_or_else(|| err500("已出账应还金额超出范围"))
            })?;
            let configured_repayment_day = detail.map(|detail| detail.repayment_day).unwrap_or(0);
            credit_accounts.push(CreditAccountSummary {
                name: account_names.get(&account.id).cloned().unwrap_or_default(),
                kind: account_kind_label(&account.kind).into(),
                balance: currency::format(balance, &account.currency),
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
                follows_billing_day: configured_repayment_day == 0,
                usage_percent: amounts.usage_percent,
                danger: amounts.danger,
            });
        } else {
            account_summaries.push(AccountSummary {
                name: account_names.get(&account.id).cloned().unwrap_or_default(),
                kind: account_kind_label(&account.kind).into(),
                balance: currency::format(balance, &account.currency),
            });
        }
    }
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
    for item in installment_items
        .iter()
        .filter(|item| item.paid_at.is_none() && item.due_date <= today + Duration::days(7))
    {
        let Some(plan) = reminder_plans.get(&item.plan_id) else {
            continue;
        };
        let Some(plan_bill) = reminder_bills.get(&plan.bill_id) else {
            continue;
        };
        let Some(plan_currency) = account_currencies.get(&plan.account_id) else {
            continue;
        };
        let title = if plan_bill.is_private {
            "隐私账单".into()
        } else {
            let category = crypto::decrypt_string(&dek, &plan_bill.category);
            let note = crypto::decrypt_string(&dek, &plan_bill.note);
            if note.is_empty() {
                category
            } else {
                format!("{category} · {note}")
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
    let category_models = category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut category_count_statuses = crate::category_limits::statuses(
        &dek,
        time_zone,
        today,
        &category_models,
        bills.iter().filter(|bill| !bill.is_private),
    )
    .map_err(err500)?;
    category_count_statuses.retain(|status| status.near_limit);
    category_count_statuses.sort_by_key(|status| {
        (
            !status.over_limit,
            !status.at_limit,
            status.remaining,
            status.id,
        )
    });

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
        let native_amount = crypto::decrypt_cents(&dek, &bill.amount);
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
        account_summaries,
        credit_accounts,
        auto_debit_warnings,
        net_assets: currency::format(net_assets, &default_currency),
        total_income: currency::format(total_income, &default_currency),
        total_expense: currency::format(total_expense, &default_currency),
        receivable: currency::format(receivable, &default_currency),
        payable: currency::format(payable, &default_currency),
        time_zone: time_zone.0.name().into(),
        subscription_reminders,
        installment_reminders,
        subscription_reminder_count,
        installment_reminder_count,
        valuation_reminders,
        valuation_reminder_count,
        budget_statuses,
        category_count_statuses,
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
}
