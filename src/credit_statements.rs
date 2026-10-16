use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::{NaiveDate, NaiveDateTime};

use crate::{
    credit_dates::{installment_billing_close, statement_dates},
    crypto,
    entity::{
        account, account_detail, balance_adjustment, bill, debt_record, installment_item,
        installment_plan, transfer,
    },
    handlers::{transfer_to_cents, ClientTimeZone},
};

pub(crate) struct Ledger<'a> {
    pub bills: &'a [bill::Model],
    pub transfers: &'a [transfer::Model],
    pub debts: &'a [debt_record::Model],
    pub adjustments: &'a [balance_adjustment::Model],
    pub plans: &'a HashMap<i64, installment_plan::Model>,
    pub items: &'a [installment_item::Model],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DueAmount {
    pub due_date: NaiveDate,
    pub ordinary: i64,
    pub installment: i64,
    pub total: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Statement {
    pub unbilled: i64,
    pub installment_principal: i64,
    pub next_installment: Option<DueAmount>,
    pub due: Vec<DueAmount>,
    pub has_adjustments: bool,
    pub has_future_entries: bool,
}

fn overflow() -> String {
    "信用账单金额超出范围".into()
}

fn add(left: i64, right: i64) -> Result<i64, String> {
    left.checked_add(right).ok_or_else(overflow)
}

fn sub(left: i64, right: i64) -> Result<i64, String> {
    left.checked_sub(right).ok_or_else(overflow)
}

fn signed(amount: i64, incoming: bool) -> Result<i64, String> {
    if incoming {
        Ok(amount)
    } else {
        amount.checked_neg().ok_or_else(overflow)
    }
}

struct Event {
    happened_at: NaiveDateTime,
    source: u8,
    id: i64,
    delta: i64,
}

struct Outstanding {
    close: NaiveDate,
    due_date: NaiveDate,
    amount: i64,
}

fn merge_due(
    due: &mut BTreeMap<NaiveDate, DueAmount>,
    date: NaiveDate,
    ordinary: i64,
    installment: i64,
) -> Result<(), String> {
    let group = due.entry(date).or_insert(DueAmount {
        due_date: date,
        ordinary: 0,
        installment: 0,
        total: 0,
    });
    group.ordinary = add(group.ordinary, ordinary)?;
    group.installment = add(group.installment, installment)?;
    group.total = add(group.ordinary, group.installment)?;
    Ok(())
}

/// 只生成估算快照，不修改账户余额、分期状态或流水。
pub(crate) fn calculate(
    account: &account::Model,
    detail: Option<&account_detail::Model>,
    ledger: &Ledger<'_>,
    dek: &crypto::Dek,
    time_zone: ClientTimeZone,
    today: NaiveDate,
) -> Result<Statement, String> {
    let mut statement = Statement::default();
    let installment_bills: HashSet<i64> = ledger
        .plans
        .values()
        .filter(|plan| plan.account_id == account.id)
        .map(|plan| plan.bill_id)
        .collect();
    let principal_transfers: HashSet<i64> = ledger
        .items
        .iter()
        .filter(|item| {
            ledger
                .plans
                .get(&item.plan_id)
                .is_some_and(|plan| plan.account_id == account.id)
        })
        .filter_map(|item| item.principal_transfer_id)
        .collect();
    let mut eligible_installment_bills = HashSet::new();
    let mut events = Vec::new();

    for bill in ledger
        .bills
        .iter()
        .filter(|bill| bill.account_id == account.id)
    {
        if time_zone.date(bill.happened_at) > today {
            statement.has_future_entries = true;
            continue;
        }
        if installment_bills.contains(&bill.id) {
            eligible_installment_bills.insert(bill.id);
            continue;
        }
        events.push(Event {
            happened_at: bill.happened_at,
            source: 1,
            id: bill.id,
            delta: signed(
                crypto::decrypt_cents(dek, &bill.amount),
                bill.kind == "income",
            )?,
        });
    }
    for transfer in ledger.transfers.iter().filter(|transfer| {
        transfer.from_account_id == account.id || transfer.to_account_id == account.id
    }) {
        if time_zone.date(transfer.happened_at) > today {
            statement.has_future_entries = true;
            continue;
        }
        if principal_transfers.contains(&transfer.id) {
            continue;
        }
        let delta = if transfer.to_account_id == account.id {
            transfer_to_cents(dek, transfer)
        } else {
            signed(crypto::decrypt_cents(dek, &transfer.amount), false)?
        };
        events.push(Event {
            happened_at: transfer.happened_at,
            source: 2,
            id: transfer.id,
            delta,
        });
    }
    for debt in ledger
        .debts
        .iter()
        .filter(|debt| debt.account_id == account.id)
    {
        if time_zone.date(debt.happened_at) > today {
            statement.has_future_entries = true;
            continue;
        }
        events.push(Event {
            happened_at: debt.happened_at,
            source: 3,
            id: debt.id,
            delta: signed(
                crypto::decrypt_cents(dek, &debt.amount),
                matches!(debt.kind.as_str(), "borrow" | "repayment_received"),
            )?,
        });
    }

    // 当前 offset 已包含所有审计差额；先逆推出初始值，再按审计发生时间重放。
    // 即使审计发生在未来，也必须先从 offset 中扣除，之后不将其计入今日估算。
    let mut adjustment_sum = 0;
    for adjustment in ledger
        .adjustments
        .iter()
        .filter(|item| item.account_id == account.id)
    {
        statement.has_adjustments = true;
        let delta = sub(
            crypto::decrypt_cents(dek, &adjustment.to_balance),
            crypto::decrypt_cents(dek, &adjustment.from_balance),
        )?;
        adjustment_sum = add(adjustment_sum, delta)?;
        if time_zone.date(adjustment.happened_at) > today {
            statement.has_future_entries = true;
            continue;
        }
        events.push(Event {
            happened_at: adjustment.happened_at,
            source: 4,
            id: adjustment.id,
            delta,
        });
    }
    let initial_offset = sub(
        crypto::decrypt_cents(dek, &account.balance_offset),
        adjustment_sum,
    )?;
    if initial_offset != 0 {
        statement.has_adjustments = true;
        let happened_at = account.created_at.naive_utc();
        if time_zone.date(happened_at) > today {
            statement.has_future_entries = true;
        } else {
            events.push(Event {
                happened_at,
                source: 0,
                id: account.id,
                delta: initial_offset,
            });
        }
    }

    // 同一 UTC 时间使用固定来源顺序和主键，数据库返回顺序不影响 FIFO。
    events.sort_unstable_by_key(|event| (event.happened_at, event.source, event.id));
    let mut outstanding: VecDeque<Outstanding> = VecDeque::new();
    let mut outstanding_total = 0;
    let mut prepayment = 0;
    for event in events {
        if event.delta >= 0 {
            let mut credit = event.delta;
            while credit > 0 {
                let Some(oldest) = outstanding.front_mut() else {
                    break;
                };
                let paid = credit.min(oldest.amount);
                oldest.amount = sub(oldest.amount, paid)?;
                outstanding_total = sub(outstanding_total, paid)?;
                credit = sub(credit, paid)?;
                if oldest.amount == 0 {
                    outstanding.pop_front();
                }
            }
            prepayment = add(prepayment, credit)?;
        } else {
            let amount = signed(event.delta, false)?;
            let prepaid = prepayment.min(amount);
            prepayment = sub(prepayment, prepaid)?;
            let remaining = sub(amount, prepaid)?;
            if remaining > 0 {
                outstanding_total = add(outstanding_total, remaining)?;
                let (close, due_date) = statement_dates(time_zone.date(event.happened_at), detail)?;
                outstanding.push_back(Outstanding {
                    close,
                    due_date,
                    amount: remaining,
                });
            }
        }
    }

    let mut due = BTreeMap::new();
    for debit in outstanding {
        if debit.close <= today {
            merge_due(&mut due, debit.due_date, debit.amount, 0)?;
        } else {
            statement.unbilled = add(statement.unbilled, debit.amount)?;
        }
    }
    for item in ledger.items.iter().filter(|item| item.paid_at.is_none()) {
        let Some(plan) = ledger.plans.get(&item.plan_id) else {
            continue;
        };
        if plan.account_id != account.id || !eligible_installment_bills.contains(&plan.bill_id) {
            continue;
        }
        statement.installment_principal = add(
            statement.installment_principal,
            crypto::decrypt_cents(dek, &item.principal),
        )?;
        let total = crypto::decrypt_cents(dek, &item.total);
        if total == 0 {
            continue;
        }
        match statement.next_installment.as_mut() {
            Some(next) if next.due_date == item.due_date => {
                next.installment = add(next.installment, total)?;
                next.total = next.installment;
            }
            Some(next) if next.due_date < item.due_date => {}
            _ => {
                statement.next_installment = Some(DueAmount {
                    due_date: item.due_date,
                    ordinary: 0,
                    installment: total,
                    total,
                });
            }
        }
        if installment_billing_close(item.due_date, detail)? <= today {
            merge_due(&mut due, item.due_date, 0, total)?;
        }
    }
    statement.due = due.into_values().collect();
    Ok(statement)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn time(value: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    struct Fixture {
        dek: crypto::Dek,
        account: account::Model,
        detail: account_detail::Model,
        bills: Vec<bill::Model>,
        transfers: Vec<transfer::Model>,
        debts: Vec<debt_record::Model>,
        adjustments: Vec<balance_adjustment::Model>,
        plans: HashMap<i64, installment_plan::Model>,
        items: Vec<installment_item::Model>,
    }

    impl Fixture {
        fn new(billing_day: i32, repayment_day: i32) -> Self {
            let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
            Self {
                account: account::Model {
                    id: 1,
                    name: String::new(),
                    kind: "credit_card".into(),
                    currency: "CNY".into(),
                    balance_offset: crypto::encrypt_cents(&dek, 0),
                    note: String::new(),
                    created_at: time("2026-01-01 00:00:00").and_utc(),
                    parent_id: None,
                    last_calibrated_at: None,
                    sms_names: String::new(),
                    transfer_fee_rate_bps: String::new(),
                },
                detail: account_detail::Model {
                    account_id: 1,
                    card_number: String::new(),
                    account_username: String::new(),
                    credit_limit: String::new(),
                    billing_day,
                    pending_billing_day: None,
                    billing_day_effective_on: None,
                    repayment_day,
                },
                dek,
                bills: Vec::new(),
                transfers: Vec::new(),
                debts: Vec::new(),
                adjustments: Vec::new(),
                plans: HashMap::new(),
                items: Vec::new(),
            }
        }

        fn bill(&mut self, id: i64, when: &str, kind: &str, cents: i64) {
            self.bills.push(bill::Model {
                id,
                account_id: 1,
                kind: kind.into(),
                amount: crypto::encrypt_cents(&self.dek, cents),
                category: String::new(),
                category_id: None,
                is_food: false,
                is_private: false,
                refund_of_bill_id: None,
                note: String::new(),
                happened_at: time(when),
                created_at: time(when).and_utc(),
            });
        }

        fn transfer(&mut self, id: i64, when: &str, incoming: bool, cents: i64, received: i64) {
            self.transfers.push(transfer::Model {
                id,
                from_account_id: if incoming { 2 } else { 1 },
                to_account_id: if incoming { 1 } else { 2 },
                amount: crypto::encrypt_cents(&self.dek, cents),
                to_amount: crypto::encrypt_cents(&self.dek, received),
                fee_rate_bps: String::new(),
                fee_amount: String::new(),
                fee_bill_id: None,
                note: String::new(),
                happened_at: time(when),
                created_at: time(when).and_utc(),
            });
        }

        fn debt(&mut self, id: i64, when: &str, kind: &str, cents: i64) {
            self.debts.push(debt_record::Model {
                id,
                person_id: 1,
                account_id: 1,
                kind: kind.into(),
                amount: crypto::encrypt_cents(&self.dek, cents),
                note: String::new(),
                happened_at: time(when),
                created_at: time(when).and_utc(),
            });
        }

        fn adjustment(&mut self, id: i64, when: &str, from: i64, to: i64) {
            self.adjustments.push(balance_adjustment::Model {
                id,
                account_id: 1,
                from_balance: crypto::encrypt_cents(&self.dek, from),
                to_balance: crypto::encrypt_cents(&self.dek, to),
                happened_at: time(when),
                created_at: time(when).and_utc(),
            });
        }

        fn plan(&mut self, id: i64, bill_id: i64, first_due: &str) {
            self.plans.insert(
                id,
                installment_plan::Model {
                    id,
                    bill_id,
                    account_id: 1,
                    term_months: 3,
                    method: "flat".into(),
                    annual_rate_bps: crypto::encrypt_cents(&self.dek, 1200),
                    fee: crypto::encrypt_cents(&self.dek, 900),
                    first_due_date: date(first_due),
                    created_at: time("2026-01-01 00:00:00").and_utc(),
                },
            );
        }

        fn item(
            &mut self,
            id: i64,
            plan_id: i64,
            due: &str,
            principal: i64,
            interest: i64,
            fee: i64,
        ) {
            self.items.push(installment_item::Model {
                id,
                plan_id,
                sequence: id as i32,
                due_date: date(due),
                principal: crypto::encrypt_cents(&self.dek, principal),
                interest: crypto::encrypt_cents(&self.dek, interest),
                fee: crypto::encrypt_cents(&self.dek, fee),
                total: crypto::encrypt_cents(&self.dek, principal + interest + fee),
                paid_at: None,
                repayment_account_id: None,
                principal_transfer_id: None,
                charge_bill_id: None,
                created_at: time("2026-01-01 00:00:00").and_utc(),
            });
        }

        fn calculate(&self, today: &str) -> Result<Statement, String> {
            self.calculate_in(today, ClientTimeZone(chrono_tz::UTC))
        }

        fn calculate_in(
            &self,
            today: &str,
            time_zone: ClientTimeZone,
        ) -> Result<Statement, String> {
            calculate(
                &self.account,
                Some(&self.detail),
                &Ledger {
                    bills: &self.bills,
                    transfers: &self.transfers,
                    debts: &self.debts,
                    adjustments: &self.adjustments,
                    plans: &self.plans,
                    items: &self.items,
                },
                &self.dek,
                time_zone,
                date(today),
            )
        }
    }

    fn due(day: &str, ordinary: i64, installment: i64) -> DueAmount {
        DueAmount {
            due_date: date(day),
            ordinary,
            installment,
            total: ordinary + installment,
        }
    }

    #[test]
    fn closing_day_is_billed_and_next_day_is_unbilled_until_its_close() {
        let mut f = Fixture::new(10, 28);
        f.bill(1, "2026-10-10 23:59:59", "expense", 60000);
        f.bill(2, "2026-10-11 00:00:00", "expense", 30000);
        let same_day = f.calculate("2026-10-10").unwrap();
        assert_eq!(same_day.due, vec![due("2026-10-28", 60000, 0)]);
        assert_eq!(same_day.unbilled, 0);
        assert!(same_day.has_future_entries);
        let statement = f.calculate("2026-10-11").unwrap();
        assert_eq!(statement.due, vec![due("2026-10-28", 60000, 0)]);
        assert_eq!(statement.unbilled, 30000);
        assert!(!statement.has_future_entries);
        let later = f.calculate("2026-11-10").unwrap();
        assert_eq!(later.unbilled, 0);
        assert_eq!(
            later.due,
            vec![due("2026-10-28", 60000, 0), due("2026-11-28", 30000, 0)]
        );
    }

    #[test]
    fn partial_repayments_pay_fifo_without_moving_overdue_dates() {
        let mut f = Fixture::new(1, 28);
        f.bill(1, "2026-08-02 00:00:00", "expense", 60000);
        f.bill(2, "2026-09-02 00:00:00", "expense", 40000);
        f.bill(3, "2026-10-02 00:00:00", "expense", 30000);
        f.transfer(1, "2026-10-10 00:00:00", true, 20000, 20000);
        let statement = f.calculate("2026-10-10").unwrap();
        assert_eq!(
            statement.due,
            vec![due("2026-09-28", 40000, 0), due("2026-10-28", 40000, 0)]
        );
        assert_eq!(statement.unbilled, 30000);
        f.bill(4, "2026-10-11 00:00:00", "income", 45000);
        assert_eq!(
            f.calculate("2026-10-11").unwrap().due,
            vec![due("2026-10-28", 35000, 0)]
        );
    }

    #[test]
    fn prepayments_carry_and_actual_incoming_currency_amount_is_used() {
        let mut f = Fixture::new(10, 28);
        f.transfer(1, "2026-09-01 00:00:00", true, 10000, 70000);
        f.bill(1, "2026-09-02 00:00:00", "expense", 40000);
        f.bill(2, "2026-10-02 00:00:00", "expense", 50000);
        assert_eq!(
            f.calculate("2026-10-10").unwrap().due,
            vec![due("2026-10-28", 20000, 0)]
        );
        f.transfers[0].to_amount.clear();
        assert_eq!(
            f.calculate("2026-10-10").unwrap().due,
            vec![due("2026-09-28", 30000, 0), due("2026-10-28", 50000, 0)]
        );
    }

    #[test]
    fn installment_payment_never_pays_ordinary_debt_twice() {
        let mut f = Fixture::new(1, 28);
        f.bill(1, "2026-09-02 00:00:00", "expense", 60000);
        f.bill(2, "2026-10-02 00:00:00", "expense", 30000);
        f.bill(3, "2026-10-02 00:00:00", "expense", 90000);
        f.plan(1, 3, "2026-10-28");
        f.item(1, 1, "2026-10-28", 30000, 900, 300);
        f.item(2, 1, "2026-11-28", 30000, 900, 300);
        f.item(3, 1, "2026-12-28", 30000, 900, 300);
        let statement = f.calculate("2026-10-10").unwrap();
        assert_eq!(statement.due, vec![due("2026-10-28", 60000, 31200)]);
        assert_eq!(statement.unbilled, 30000);
        assert_eq!(statement.installment_principal, 90000);
        assert_eq!(
            statement.next_installment,
            Some(due("2026-10-28", 0, 31200))
        );
        f.transfer(1, "2026-10-10 12:00:00", true, 30000, 30000);
        f.items[0].paid_at = Some(time("2026-10-10 12:00:00").and_utc());
        f.items[0].principal_transfer_id = Some(1);
        let paid = f.calculate("2026-10-10").unwrap();
        assert_eq!(paid.due, vec![due("2026-10-28", 60000, 0)]);
        assert_eq!(paid.unbilled, 30000);
        assert_eq!(paid.installment_principal, 60000);
        assert_eq!(paid.next_installment, Some(due("2026-11-28", 0, 31200)));
        f.transfer(2, "2026-10-10 13:00:00", true, 20000, 20000);
        let partial = f.calculate("2026-10-10").unwrap();
        assert_eq!(partial.due, vec![due("2026-10-28", 40000, 0)]);
        assert_eq!(partial.unbilled, 30000);
    }

    #[test]
    fn installments_merge_dates_and_include_only_closed_periods() {
        let mut f = Fixture::new(20, 5);
        f.bill(1, "2026-09-01 00:00:00", "expense", 60000);
        f.plan(1, 1, "2026-10-05");
        f.item(1, 1, "2026-10-05", 20000, 100, 50);
        f.item(2, 1, "2026-11-05", 20000, 100, 50);
        f.item(3, 1, "2026-12-05", 20000, 100, 50);
        f.bill(2, "2026-09-02 00:00:00", "expense", 10000);
        f.plan(2, 2, "2026-10-05");
        f.item(4, 2, "2026-10-05", 10000, 200, 75);
        let before = f.calculate("2026-09-19").unwrap();
        assert!(before.due.is_empty());
        assert_eq!(before.next_installment, Some(due("2026-10-05", 0, 30425)));
        let closed = f.calculate("2026-09-20").unwrap();
        assert_eq!(closed.due, vec![due("2026-10-05", 0, 30425)]);
        let overdue = f.calculate("2026-10-20").unwrap();
        assert_eq!(
            overdue.due,
            vec![due("2026-10-05", 0, 30425), due("2026-11-05", 0, 20150)]
        );
        assert_eq!(overdue.installment_principal, 70000);
    }

    #[test]
    fn timezone_and_scheduled_changes_determine_statement_month() {
        let mut f = Fixture::new(5, 12);
        f.detail.pending_billing_day = Some(20);
        f.detail.billing_day_effective_on = Some(date("2026-11-01"));
        f.bill(1, "2026-10-05 16:00:00", "expense", 10000);
        let utc = f.calculate("2026-10-06").unwrap();
        assert_eq!(utc.due, vec![due("2026-10-12", 10000, 0)]);
        let shanghai = ClientTimeZone(chrono_tz::Asia::Shanghai);
        let local = f.calculate_in("2026-10-06", shanghai).unwrap();
        assert_eq!(local.unbilled, 10000);
        assert!(local.due.is_empty());
        assert_eq!(
            f.calculate_in("2026-11-20", shanghai).unwrap().due,
            vec![due("2026-12-12", 10000, 0)]
        );
    }

    #[test]
    fn audit_offsets_are_reconstructed_then_replayed_not_double_counted() {
        let mut f = Fixture::new(10, 28);
        // 初始 -100 元，9 月增加欠款 50 元，10 月调高余额 80 元。
        f.account.balance_offset = crypto::encrypt_cents(&f.dek, -7000);
        f.adjustment(1, "2026-09-02 00:00:00", -10000, -15000);
        f.adjustment(2, "2026-10-02 00:00:00", -15000, -7000);
        let statement = f.calculate("2026-10-10").unwrap();
        assert!(statement.has_adjustments);
        assert_eq!(
            statement.due,
            vec![due("2026-01-28", 2000, 0), due("2026-09-28", 5000, 0)]
        );
        assert!(!statement.has_future_entries);
    }

    #[test]
    fn future_entries_are_flagged_and_excluded_including_offset_and_plan() {
        let mut f = Fixture::new(10, 28);
        f.account.balance_offset = crypto::encrypt_cents(&f.dek, -15000);
        f.adjustment(1, "2026-11-01 00:00:00", -10000, -15000);
        f.bill(1, "2026-11-01 00:00:00", "expense", 30000);
        f.plan(1, 1, "2026-11-28");
        f.item(1, 1, "2026-11-28", 30000, 100, 50);
        f.transfer(1, "2026-11-02 00:00:00", true, 5000, 5000);
        f.debt(1, "2026-11-03 00:00:00", "lend", 7000);
        let statement = f.calculate("2026-10-10").unwrap();
        assert_eq!(statement.due, vec![due("2026-01-28", 10000, 0)]);
        assert_eq!(statement.installment_principal, 0);
        assert!(statement.next_installment.is_none());
        assert!(statement.has_adjustments && statement.has_future_entries);
        f.account.balance_offset = crypto::encrypt_cents(&f.dek, -5000);
        assert!(f.calculate("2026-10-10").unwrap().due.is_empty());
    }

    #[test]
    fn ordinary_outgoing_and_debt_directions_follow_existing_balance_rules() {
        let mut f = Fixture::new(10, 28);
        f.transfer(1, "2026-10-01 00:00:00", false, 10000, 9000);
        f.debt(1, "2026-10-02 00:00:00", "lend", 20000);
        f.debt(2, "2026-10-03 00:00:00", "repayment_paid", 30000);
        f.debt(3, "2026-10-04 00:00:00", "borrow", 5000);
        f.debt(4, "2026-10-05 00:00:00", "repayment_received", 8000);
        assert_eq!(
            f.calculate("2026-10-10").unwrap().due,
            vec![due("2026-10-28", 47000, 0)]
        );
    }

    #[test]
    fn same_timestamp_order_is_stable_by_source_and_id() {
        let mut f = Fixture::new(10, 28);
        f.bill(2, "2026-09-02 00:00:00", "expense", 20000);
        f.bill(1, "2026-09-02 00:00:00", "income", 10000);
        f.bill(3, "2026-10-02 00:00:00", "expense", 30000);
        f.transfer(1, "2026-10-03 00:00:00", true, 15000, 15000);
        let expected = f.calculate("2026-10-10").unwrap();
        f.bills.reverse();
        assert_eq!(f.calculate("2026-10-10").unwrap(), expected);
        assert_eq!(expected.due, vec![due("2026-10-28", 25000, 0)]);
    }

    #[test]
    fn checked_cents_reject_debt_credit_adjustment_and_due_overflow() {
        let mut f = Fixture::new(10, 28);
        f.bill(1, "2026-10-01 00:00:00", "expense", i64::MIN);
        assert!(f.calculate("2026-10-10").is_err());
        f.bills.clear();
        f.bill(1, "2026-10-01 00:00:00", "expense", i64::MAX);
        f.bill(2, "2026-10-02 00:00:00", "expense", 1);
        assert!(f.calculate("2026-10-10").is_err());
        f.bills.clear();
        f.bill(1, "2026-10-01 00:00:00", "income", i64::MAX);
        f.bill(2, "2026-10-02 00:00:00", "income", 1);
        assert!(f.calculate("2026-10-10").is_err());
        f.bills.clear();
        f.adjustment(1, "2026-10-01 00:00:00", i64::MIN, i64::MAX);
        assert!(f.calculate("2026-10-10").is_err());
        f.adjustments.clear();
        f.bill(1, "2026-10-01 00:00:00", "expense", i64::MAX);
        f.bill(2, "2026-10-01 00:00:00", "expense", 1);
        f.plan(1, 2, "2026-10-28");
        f.item(1, 1, "2026-10-28", 1, 0, 0);
        assert!(f.calculate("2026-10-10").is_err());
    }
}
