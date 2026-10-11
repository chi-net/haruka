use chrono::{Datelike, Duration, Months, NaiveDate};

use crate::entity::account_detail;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillingDayEffect {
    NextMonth,
    Immediate,
}

impl BillingDayEffect {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "next_month" => Some(Self::NextMonth),
            "immediate" => Some(Self::Immediate),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BillingSchedule {
    pub billing_day: i32,
    pub pending_billing_day: Option<i32>,
    pub billing_day_effective_on: Option<NaiveDate>,
}

pub fn effective_billing_day(detail: &account_detail::Model, on: NaiveDate) -> i32 {
    match (detail.pending_billing_day, detail.billing_day_effective_on) {
        (Some(day), Some(effective_on)) if on >= effective_on => day,
        _ => detail.billing_day,
    }
}

pub fn pending_billing_change(
    detail: &account_detail::Model,
    on: NaiveDate,
) -> Option<(i32, NaiveDate)> {
    match (detail.pending_billing_day, detail.billing_day_effective_on) {
        (Some(day), Some(effective_on)) if on < effective_on => Some((day, effective_on)),
        _ => None,
    }
}

fn next_month_start(on: NaiveDate) -> NaiveDate {
    on.with_day(1)
        .expect("有效月初")
        .checked_add_months(Months::new(1))
        .expect("有效下月")
}

impl BillingSchedule {
    /// 新信用账户立即使用输入值；编辑先解析成熟计划，再保留、替换或取消未来计划。
    pub fn for_edit(
        existing: Option<&account_detail::Model>,
        submitted_day: i32,
        effect: BillingDayEffect,
        today: NaiveDate,
    ) -> Self {
        let Some(detail) = existing else {
            return Self {
                billing_day: submitted_day,
                ..Self::default()
            };
        };
        let current = effective_billing_day(detail, today);
        if effect == BillingDayEffect::Immediate || submitted_day == current {
            return Self {
                billing_day: submitted_day,
                ..Self::default()
            };
        }
        let effective_on = match pending_billing_change(detail, today) {
            Some((day, effective_on)) if day == submitted_day => effective_on,
            _ => next_month_start(today),
        };
        Self {
            billing_day: current,
            pending_billing_day: Some(submitted_day),
            billing_day_effective_on: Some(effective_on),
        }
    }
}

fn date_with_clamped_day(month_start: NaiveDate, day: i32) -> NaiveDate {
    let last_day = (next_month_start(month_start) - Duration::days(1)).day();
    month_start
        .with_day((day.max(1) as u32).min(last_day))
        .expect("有效日期")
}

pub fn next_repayment_date(today: NaiveDate, detail: Option<&account_detail::Model>) -> NaiveDate {
    let repayment_in_month = |month_start| {
        let day = match detail {
            Some(detail) if detail.repayment_day > 0 => detail.repayment_day,
            Some(detail) => effective_billing_day(detail, month_start),
            None => 1,
        };
        date_with_clamped_day(month_start, day)
    };
    let month_start = today.with_day(1).expect("有效月初");
    let current = repayment_in_month(month_start);
    if current >= today {
        current
    } else {
        repayment_in_month(next_month_start(today))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn detail(day: i32, pending: Option<i32>, effective_on: Option<&str>) -> account_detail::Model {
        account_detail::Model {
            account_id: 1,
            card_number: String::new(),
            account_username: String::new(),
            credit_limit: String::new(),
            billing_day: day,
            pending_billing_day: pending,
            billing_day_effective_on: effective_on.map(date),
            repayment_day: 0,
        }
    }

    #[test]
    fn new_account_and_immediate_changes_apply_now() {
        let today = date("2026-10-10");
        assert_eq!(
            BillingSchedule::for_edit(None, 20, BillingDayEffect::NextMonth, today),
            BillingSchedule {
                billing_day: 20,
                ..BillingSchedule::default()
            }
        );
        let existing = detail(5, Some(20), Some("2026-11-01"));
        assert_eq!(
            BillingSchedule::for_edit(Some(&existing), 20, BillingDayEffect::Immediate, today),
            BillingSchedule {
                billing_day: 20,
                ..BillingSchedule::default()
            }
        );
        assert_eq!(
            BillingSchedule::for_edit(Some(&existing), 12, BillingDayEffect::Immediate, today),
            BillingSchedule {
                billing_day: 12,
                ..BillingSchedule::default()
            }
        );
    }

    #[test]
    fn deferred_changes_replace_preserve_or_cancel() {
        let today = date("2026-12-31");
        let existing = detail(5, None, None);
        let schedule =
            BillingSchedule::for_edit(Some(&existing), 20, BillingDayEffect::NextMonth, today);
        assert_eq!(
            schedule,
            BillingSchedule {
                billing_day: 5,
                pending_billing_day: Some(20),
                billing_day_effective_on: Some(date("2027-01-01")),
            }
        );
        let existing = detail(5, Some(20), Some("2027-01-01"));
        assert_eq!(
            BillingSchedule::for_edit(Some(&existing), 20, BillingDayEffect::NextMonth, today),
            schedule
        );
        let replacement =
            BillingSchedule::for_edit(Some(&existing), 12, BillingDayEffect::NextMonth, today);
        assert_eq!(replacement.billing_day, 5);
        assert_eq!(replacement.pending_billing_day, Some(12));
        assert_eq!(
            replacement.billing_day_effective_on,
            Some(date("2027-01-01"))
        );
        assert_eq!(
            BillingSchedule::for_edit(Some(&existing), 5, BillingDayEffect::NextMonth, today),
            BillingSchedule {
                billing_day: 5,
                ..BillingSchedule::default()
            }
        );
        let existing = detail(5, Some(20), Some("2027-02-01"));
        let preserved =
            BillingSchedule::for_edit(Some(&existing), 20, BillingDayEffect::NextMonth, today);
        assert_eq!(preserved.billing_day, 5);
        assert_eq!(preserved.pending_billing_day, Some(20));
        assert_eq!(preserved.billing_day_effective_on, Some(date("2027-02-01")));
    }

    #[test]
    fn pending_activates_on_date_and_save_normalizes_without_rescheduling() {
        let existing = detail(5, Some(20), Some("2026-11-01"));
        assert_eq!(effective_billing_day(&existing, date("2026-10-31")), 5);
        assert_eq!(effective_billing_day(&existing, date("2026-11-01")), 20);
        assert_eq!(pending_billing_change(&existing, date("2026-11-01")), None);
        assert_eq!(
            BillingSchedule::for_edit(
                Some(&existing),
                20,
                BillingDayEffect::NextMonth,
                date("2026-11-10")
            ),
            BillingSchedule {
                billing_day: 20,
                ..BillingSchedule::default()
            }
        );
        assert_eq!(
            BillingSchedule::for_edit(
                Some(&existing),
                12,
                BillingDayEffect::NextMonth,
                date("2026-11-10")
            ),
            BillingSchedule {
                billing_day: 20,
                pending_billing_day: Some(12),
                billing_day_effective_on: Some(date("2026-12-01"))
            }
        );
    }

    #[test]
    fn repayment_follows_current_and_pending_month_but_fixed_day_does_not() {
        let mut existing = detail(5, Some(20), Some("2026-11-01"));
        assert_eq!(
            next_repayment_date(date("2026-10-01"), Some(&existing)),
            date("2026-10-05")
        );
        assert_eq!(
            next_repayment_date(date("2026-10-05"), Some(&existing)),
            date("2026-10-05")
        );
        assert_eq!(
            next_repayment_date(date("2026-10-06"), Some(&existing)),
            date("2026-11-20")
        );
        assert_eq!(
            next_repayment_date(date("2026-11-01"), Some(&existing)),
            date("2026-11-20")
        );
        existing.repayment_day = 12;
        assert_eq!(
            next_repayment_date(date("2026-10-13"), Some(&existing)),
            date("2026-11-12")
        );
        assert_eq!(
            next_repayment_date(date("2026-11-01"), Some(&existing)),
            date("2026-11-12")
        );
    }

    #[test]
    fn repayment_clamps_month_end_including_leap_year_and_year_boundary() {
        let existing = detail(31, None, None);
        assert_eq!(
            next_repayment_date(date("2024-02-01"), Some(&existing)),
            date("2024-02-29")
        );
        assert_eq!(
            next_repayment_date(date("2026-02-01"), Some(&existing)),
            date("2026-02-28")
        );
        assert_eq!(
            next_repayment_date(date("2026-04-01"), Some(&existing)),
            date("2026-04-30")
        );
        let existing = detail(5, Some(31), Some("2024-02-01"));
        assert_eq!(
            next_repayment_date(date("2024-01-31"), Some(&existing)),
            date("2024-02-29")
        );
        let existing = detail(31, Some(2), Some("2026-11-01"));
        assert_eq!(
            next_repayment_date(date("2026-10-30"), Some(&existing)),
            date("2026-10-31")
        );
        assert_eq!(
            next_repayment_date(date("2026-11-01"), Some(&existing)),
            date("2026-11-02")
        );
        let existing = detail(5, Some(2), Some("2027-01-01"));
        assert_eq!(
            next_repayment_date(date("2026-12-31"), Some(&existing)),
            date("2027-01-02")
        );
        assert_eq!(
            next_repayment_date(date("2026-12-31"), None),
            date("2027-01-01")
        );
    }

    #[test]
    fn effect_mode_only_accepts_exclusive_supported_choices() {
        assert_eq!(
            BillingDayEffect::parse("next_month"),
            Some(BillingDayEffect::NextMonth)
        );
        assert_eq!(
            BillingDayEffect::parse("immediate"),
            Some(BillingDayEffect::Immediate)
        );
        assert_eq!(BillingDayEffect::parse(""), None);
        assert_eq!(BillingDayEffect::parse("later"), None);
    }
}
