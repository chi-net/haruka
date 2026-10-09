use askama::Template;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{Html, Redirect},
};
use chrono::{Datelike, Duration, Months, NaiveDate, Timelike};
use rust_decimal::Decimal;
use sea_orm::{EntityTrait, QueryOrder};
use serde::Serialize;
use std::collections::HashMap;

use crate::{
    crypto, currency,
    entity::{
        account, account_detail, bill, category, debt_person, debt_record, installment_item,
        installment_plan, subscription,
    },
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

struct AccountOption {
    id: i64,
    name: String,
    kind: String,
    currency: String,
}

struct AccountSummary {
    name: String,
    kind: String,
    balance: String,
}

struct CreditAccountSummary {
    name: String,
    kind: String,
    balance: String,
    used: String,
    available: String,
    limit: String,
    repayment_date: String,
    repayment_amount: String,
    follows_billing_day: bool,
    usage_percent: i64,
    danger: bool,
}

struct PersonOption {
    id: i64,
    name: String,
}

struct CategoryOption {
    kind: String,
    name: String,
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

#[derive(Serialize)]
struct ReportSeries {
    labels: Vec<String>,
    income: Vec<i64>,
    expense: Vec<i64>,
}

#[derive(Serialize)]
struct Reports {
    daily: ReportSeries,
    weekly: ReportSeries,
    monthly: ReportSeries,
    yearly: ReportSeries,
}

struct BillValue {
    happened_at: chrono::DateTime<chrono_tz::Tz>,
    kind: String,
    amount: i64,
    is_food: bool,
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
    accounts: Vec<AccountOption>,
    transfer_sources: Vec<AccountOption>,
    account_summaries: Vec<AccountSummary>,
    credit_accounts: Vec<CreditAccountSummary>,
    auto_debit_warnings: Vec<AutoDebitWarning>,
    people: Vec<PersonOption>,
    categories: Vec<CategoryOption>,
    quick_entry_heading: String,
    quick_redirect_to: String,
    happened_at: String,
    net_assets: String,
    month_income: String,
    month_expense: String,
    receivable: String,
    payable: String,
    engel_coefficient: String,
    food_expense: String,
    reports_json: String,
    default_currency: String,
    time_zone: String,
    first_due_date: String,
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

fn date_with_clamped_day(year: i32, month: u32, day: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("有效年月");
    let next_month = first.checked_add_months(Months::new(1)).expect("有效年月");
    let last_day = (next_month - Duration::days(1)).day();
    NaiveDate::from_ymd_opt(year, month, day.min(last_day)).expect("有效日期")
}

fn next_repayment_date(today: NaiveDate, repayment_day: i32) -> NaiveDate {
    let current = date_with_clamped_day(today.year(), today.month(), repayment_day as u32);
    if current >= today {
        current
    } else {
        let next_month = today.checked_add_months(Months::new(1)).expect("有效年月");
        date_with_clamped_day(next_month.year(), next_month.month(), repayment_day as u32)
    }
}

fn add_value(series: &mut ReportSeries, index: usize, bill: &BillValue) -> HandlerResult<()> {
    let target = if bill.kind == "income" {
        &mut series.income[index]
    } else {
        &mut series.expense[index]
    };
    *target = target
        .checked_add(bill.amount)
        .ok_or_else(|| err500("报表金额超出范围"))?;
    Ok(())
}

fn build_date_series(
    today: NaiveDate,
    days: i64,
    bills: &[BillValue],
) -> HandlerResult<ReportSeries> {
    let dates = (0..days)
        .map(|offset| today - Duration::days(days - 1 - offset))
        .collect::<Vec<_>>();
    let mut series = ReportSeries {
        labels: dates
            .iter()
            .map(|date| date.format("%m-%d").to_string())
            .collect(),
        income: vec![0; dates.len()],
        expense: vec![0; dates.len()],
    };
    let indexes: HashMap<NaiveDate, usize> = dates
        .iter()
        .enumerate()
        .map(|(index, date)| (*date, index))
        .collect();
    for bill in bills {
        if let Some(index) = indexes.get(&bill.happened_at.date_naive()) {
            add_value(&mut series, *index, bill)?;
        }
    }
    Ok(series)
}

fn build_reports(today: NaiveDate, bills: &[BillValue]) -> HandlerResult<Reports> {
    let mut daily = ReportSeries {
        labels: (0..24).map(|hour| format!("{hour:02}:00")).collect(),
        income: vec![0; 24],
        expense: vec![0; 24],
    };
    for bill in bills {
        if bill.happened_at.date_naive() == today {
            add_value(&mut daily, bill.happened_at.hour() as usize, bill)?;
        }
    }
    Ok(Reports {
        daily,
        weekly: build_date_series(today, 7, bills)?,
        monthly: build_date_series(today, 30, bills)?,
        yearly: build_date_series(today, 365, bills)?,
    })
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
) -> HandlerResult<Html<String>> {
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let today = time_zone.today();
    let default_currency = currency::default_currency(&state).await.map_err(err500)?;
    let currencies = accounts
        .iter()
        .map(|account| account.currency.clone())
        .collect::<Vec<_>>();
    let rates = currency::RateTable::load(&state, currencies, &default_currency, today)
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
    let account_names = super::bills::account_display_names(&dek, &accounts, &details);
    let account_options = accounts
        .iter()
        .filter(|account| crate::investment_funds::is_money_account(account))
        .map(|account| AccountOption {
            id: account.id,
            name: account_names.get(&account.id).cloned().unwrap_or_default(),
            kind: account.kind.clone(),
            currency: account.currency.clone(),
        })
        .collect::<Vec<_>>();
    let mut account_balances = HashMap::new();
    for account in accounts
        .iter()
        .filter(|account| crate::investment_funds::is_money_account(account))
    {
        let balance = super::accounts::current_balance(&state, &dek, account.id).await?;
        account_balances.insert(account.id, balance);
    }
    crate::investment_funds::roll_up_balances(&accounts, &mut account_balances)?;
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
            let detail = details.get(&account.id);
            let limit = detail
                .map(|detail| crypto::decrypt_cents(&dek, &detail.credit_limit))
                .unwrap_or_default();
            let used = balance.checked_neg().unwrap_or(i64::MAX).max(0);
            let available = limit.saturating_sub(used).max(0);
            let usage_percent = if limit > 0 {
                ((i128::from(used) * 100 / i128::from(limit)).min(100)) as i64
            } else if used > 0 {
                100
            } else {
                0
            };
            let billing_day = detail.map(|detail| detail.billing_day).unwrap_or(1).max(1);
            let configured_repayment_day = detail.map(|detail| detail.repayment_day).unwrap_or(0);
            let repayment_day = if configured_repayment_day > 0 {
                configured_repayment_day
            } else {
                billing_day
            };
            credit_accounts.push(CreditAccountSummary {
                name: account_names.get(&account.id).cloned().unwrap_or_default(),
                kind: account_kind_label(&account.kind).into(),
                balance: currency::format(balance, &account.currency),
                used: currency::format(used, &account.currency),
                available: currency::format(available, &account.currency),
                limit: currency::format(limit, &account.currency),
                repayment_date: next_repayment_date(today, repayment_day)
                    .format("%m月%d日")
                    .to_string(),
                repayment_amount: currency::format(used, &account.currency),
                follows_billing_day: configured_repayment_day == 0,
                usage_percent,
                danger: available == 0 || (limit > 0 && available <= limit / 5),
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

    let reminder_plans: HashMap<i64, installment_plan::Model> = installment_plan::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|plan| (plan.id, plan))
        .collect();
    let reminder_bills: HashMap<i64, bill::Model> = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| (bill.id, bill))
        .collect();
    let mut all_installment_reminders = Vec::new();
    for item in installment_item::Entity::find()
        .order_by_asc(installment_item::Column::DueDate)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
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
        let category = crypto::decrypt_string(&dek, &plan_bill.category);
        let note = crypto::decrypt_string(&dek, &plan_bill.note);
        all_installment_reminders.push(ReminderRow {
            title: if note.is_empty() {
                category
            } else {
                format!("{category} · {note}")
            },
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
    let transfer_sources = accounts
        .iter()
        .filter(|account| crate::investment_funds::is_money_account(account))
        .map(|account| AccountOption {
            id: account.id,
            name: account_names.get(&account.id).cloned().unwrap_or_default(),
            kind: account.kind.clone(),
            currency: account.currency.clone(),
        })
        .collect::<Vec<_>>();
    let people = debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let people_options = people
        .iter()
        .map(|person| PersonOption {
            id: person.id,
            name: crypto::decrypt_string(&dek, &person.name),
        })
        .collect::<Vec<_>>();
    let categories = category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|category| CategoryOption {
            kind: category.kind,
            name: crypto::decrypt_string(&dek, &category.name),
        })
        .collect::<Vec<_>>();

    let debt_records = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut receivable = 0i64;
    let mut payable = 0i64;
    for record in debt_records {
        let native_amount = crypto::decrypt_cents(&dek, &record.amount);
        let record_currency = account_currencies
            .get(&record.account_id)
            .map(String::as_str)
            .unwrap_or(&default_currency);
        let amount = rates
            .convert(native_amount, record_currency)
            .map_err(err500)?;
        match record.kind.as_str() {
            "lend" => {
                receivable = receivable
                    .checked_add(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            "repayment_received" => {
                receivable = receivable
                    .checked_sub(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            "borrow" => {
                payable = payable
                    .checked_add(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            "repayment_paid" => {
                payable = payable
                    .checked_sub(amount)
                    .ok_or_else(|| err500("借贷金额超出范围"))?
            }
            _ => {}
        }
    }

    let bill_values = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| {
            let native_amount = crypto::decrypt_cents(&dek, &bill.amount);
            let bill_currency = account_currencies
                .get(&bill.account_id)
                .map(String::as_str)
                .unwrap_or(&default_currency);
            Ok(BillValue {
                happened_at: time_zone.local_datetime(bill.happened_at),
                kind: bill.kind,
                amount: rates
                    .convert(native_amount, bill_currency)
                    .map_err(err500)?,
                is_food: bill.is_food,
            })
        })
        .collect::<HandlerResult<Vec<_>>>()?;
    let mut month_income = 0i64;
    let mut month_expense = 0i64;
    let mut food_expense = 0i64;
    for bill in &bill_values {
        let date = bill.happened_at.date_naive();
        if date <= today && date.year() == today.year() && date.month() == today.month() {
            if bill.kind == "income" {
                month_income = month_income
                    .checked_add(bill.amount)
                    .ok_or_else(|| err500("报表金额超出范围"))?;
            } else {
                month_expense = month_expense
                    .checked_add(bill.amount)
                    .ok_or_else(|| err500("报表金额超出范围"))?;
                if bill.is_food {
                    food_expense = food_expense
                        .checked_add(bill.amount)
                        .ok_or_else(|| err500("报表金额超出范围"))?;
                }
            }
        }
    }
    let engel_coefficient = if month_expense == 0 {
        "—".into()
    } else {
        format!(
            "{}%",
            (Decimal::from(food_expense) * Decimal::from(100) / Decimal::from(month_expense))
                .round_dp(1)
        )
    };
    let reports_json =
        serde_json::to_string(&build_reports(today, &bill_values)?).map_err(err500)?;
    let budget_statuses = super::budgets::current_statuses(&state, &dek, time_zone).await?;
    let html = DashboardTemplate {
        accounts: account_options,
        transfer_sources,
        account_summaries,
        credit_accounts,
        auto_debit_warnings,
        people: people_options,
        categories,
        quick_entry_heading: "快速记账".into(),
        quick_redirect_to: "/dashboard".into(),
        happened_at: chrono::Utc::now().naive_utc().format(TIME_FMT).to_string(),
        net_assets: currency::format(net_assets, &default_currency),
        month_income: currency::format(month_income, &default_currency),
        month_expense: currency::format(month_expense, &default_currency),
        receivable: currency::format(receivable, &default_currency),
        payable: currency::format(payable, &default_currency),
        engel_coefficient,
        food_expense: currency::format(food_expense, &default_currency),
        reports_json,
        default_currency,
        time_zone: time_zone.0.name().into(),
        first_due_date: (today + chrono::Months::new(1))
            .format("%Y-%m-%d")
            .to_string(),
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
    fn repayment_date_keeps_today_and_rolls_after_due_day() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        assert_eq!(next_repayment_date(today, 10), today);
        assert_eq!(
            next_repayment_date(today, 9),
            NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()
        );
    }

    #[test]
    fn repayment_date_clamps_to_month_end() {
        let today = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        assert_eq!(
            next_repayment_date(today, 31),
            NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()
        );
    }
}
