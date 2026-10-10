use std::{collections::HashSet, str::FromStr};

use chrono::{Datelike, Months, NaiveDate};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use serde::{Deserialize, Serialize};

use crate::{currency, market_data};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Config {
    pub name: String,
    pub currency: String,
    pub income_months: u32,
    pub manual_monthly_income: String,
    pub investment_bps: i64,
    pub emergency_months: u32,
    pub trading_days: u32,
    pub from_account_id: Option<i64>,
    pub parent_account_id: Option<i64>,
    pub reserve_account_ids: Vec<i64>,
    pub start_date: NaiveDate,
    pub allocations: Vec<Allocation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Allocation {
    pub key: String,
    pub name: String,
    pub asset_class: String,
    pub share_bps: i64,
    pub fund_account_id: Option<i64>,
    pub strategy: String,
    pub index_code: String,
    pub moving_average_days: i32,
    pub fee_rate_bps: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LinkedPlan {
    pub key: String,
    pub plan_id: i64,
    pub fund_account_id: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Saved {
    pub config: Config,
    pub linked_plans: Vec<LinkedPlan>,
}

pub(crate) fn default_config(currency: String, start_date: NaiveDate) -> Config {
    Config {
        name: "我的理财规划".into(),
        currency,
        income_months: 3,
        manual_monthly_income: String::new(),
        investment_bps: 1_000,
        emergency_months: 6,
        trading_days: 20,
        from_account_id: None,
        parent_account_id: None,
        reserve_account_ids: Vec::new(),
        start_date,
        allocations: [("overseas", "纳斯达克 100"), ("domestic", "沪港深 500")]
            .into_iter()
            .map(|(key, name)| Allocation {
                key: key.into(),
                name: name.into(),
                asset_class: "equity".into(),
                share_bps: 5_000,
                fund_account_id: None,
                strategy: "fixed".into(),
                index_code: String::new(),
                moving_average_days: 180,
                fee_rate_bps: 0,
            })
            .collect(),
    }
}

pub(crate) fn validate_config(config: &Config) -> Result<(), String> {
    if config.name.trim().is_empty() || config.name.chars().count() > 200 {
        return Err("理财规划名称不能为空且不能超过 200 个字符".into());
    }
    if !currency::valid(&config.currency) {
        return Err("理财规划币种无效".into());
    }
    if !(1..=12).contains(&config.income_months) {
        return Err("收入均值月份数必须位于 1 到 12 之间".into());
    }
    if !config.manual_monthly_income.trim().is_empty() {
        parse_money(&config.manual_monthly_income)?;
    }
    if !(0..=10_000).contains(&config.investment_bps) {
        return Err("投资比例必须位于 0% 到 100% 之间".into());
    }
    if config.emergency_months > 60 {
        return Err("应急金月份数必须位于 0 到 60 之间".into());
    }
    if !(1..=31).contains(&config.trading_days) {
        return Err("预计交易日数必须位于 1 到 31 之间".into());
    }
    if config.from_account_id.is_some_and(|id| id <= 0)
        || config.parent_account_id.is_some_and(|id| id <= 0)
    {
        return Err("扣款账户和基金分组编号必须大于 0".into());
    }
    if config.reserve_account_ids.len() > 20 {
        return Err("应急金账户不能超过 20 个".into());
    }
    let mut reserve_ids = HashSet::with_capacity(config.reserve_account_ids.len());
    for &id in &config.reserve_account_ids {
        if id <= 0 || !reserve_ids.insert(id) {
            return Err("应急金账户编号必须大于 0 且不能重复".into());
        }
    }
    if !(1..=20).contains(&config.allocations.len()) {
        return Err("配置项目数必须位于 1 到 20 之间".into());
    }
    let mut keys = HashSet::with_capacity(config.allocations.len());
    let mut fund_ids = HashSet::with_capacity(config.allocations.len());
    let mut share_total = 0_i64;
    for allocation in &config.allocations {
        if allocation.key.trim().is_empty()
            || !allocation.key.is_ascii()
            || allocation.key.len() > 80
            || !keys.insert(allocation.key.as_str())
        {
            return Err("配置项目标识必须是非空、不重复且不超过 80 字符的 ASCII 文本".into());
        }
        if allocation.name.trim().is_empty() || allocation.name.chars().count() > 200 {
            return Err("配置项目名称不能为空且不能超过 200 个字符".into());
        }
        if !matches!(
            allocation.asset_class.as_str(),
            "equity" | "bond" | "gold" | "cash" | "other"
        ) {
            return Err("配置项目资产类别无效".into());
        }
        if !(0..=10_000).contains(&allocation.share_bps) {
            return Err("配置项目比例必须位于 0% 到 100% 之间".into());
        }
        share_total += allocation.share_bps;
        if let Some(id) = allocation.fund_account_id {
            if id <= 0 || !fund_ids.insert(id) {
                return Err("绑定基金账户编号必须大于 0 且不能重复".into());
            }
        }
        if !(0..=10_000).contains(&allocation.fee_rate_bps) {
            return Err("手续费率必须位于 0% 到 100% 之间".into());
        }
        match allocation.strategy.as_str() {
            "fixed" => {}
            "smart" => {
                if market_data::index_option(&allocation.index_code).is_none() {
                    return Err("智能定投指数无效".into());
                }
                if !market_data::valid_moving_average(allocation.moving_average_days) {
                    return Err("智能定投均线天数无效".into());
                }
            }
            _ => return Err("定投策略必须为固定定投或智能定投".into()),
        }
    }
    if share_total != 10_000 {
        return Err("配置项目比例合计必须恰好为 100%".into());
    }
    Ok(())
}

pub(crate) fn parse_money(value: &str) -> Result<i64, String> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        || value.bytes().filter(|&byte| byte == b'.').count() > 1
    {
        return Err("月收入必须是有效的十进制金额".into());
    }
    if value
        .split_once('.')
        .is_some_and(|(_, fraction)| fraction.len() > 2)
    {
        return Err("月收入最多保留两位小数".into());
    }
    let amount =
        Decimal::from_str(value).map_err(|_| "月收入金额格式无效或超出范围".to_string())?;
    if amount <= Decimal::ZERO {
        return Err("月收入必须大于 0".into());
    }
    amount
        .checked_mul(Decimal::from(100))
        .and_then(|cents| cents.to_i64())
        .ok_or_else(|| "月收入金额超出范围".to_string())
}

pub(crate) fn monthly_budget(income: i64, bps: i64) -> Result<i64, String> {
    if income < 0 {
        return Err("月收入不能小于 0".into());
    }
    if !(0..=10_000).contains(&bps) {
        return Err("投资比例必须位于 0% 到 100% 之间".into());
    }
    Ok((i128::from(income) * i128::from(bps) / 10_000) as i64)
}

pub(crate) fn allocate_monthly(total: i64, shares: &[i64]) -> Result<Vec<i64>, String> {
    if total < 0 {
        return Err("月度投资金额不能小于 0".into());
    }
    if shares.iter().any(|share| !(0..=10_000).contains(share)) {
        return Err("配置项目比例必须位于 0% 到 100% 之间".into());
    }
    if shares.iter().map(|&share| i128::from(share)).sum::<i128>() != 10_000 {
        return Err("配置项目比例合计必须恰好为 100%".into());
    }
    let mut amounts = Vec::with_capacity(shares.len());
    let mut remainders = Vec::with_capacity(shares.len());
    let mut allocated = 0_i128;
    for (index, &share) in shares.iter().enumerate() {
        let product = i128::from(total) * i128::from(share);
        let amount = product / 10_000;
        amounts.push(amount as i64);
        allocated += amount;
        if share > 0 {
            remainders.push((product % 10_000, index));
        }
    }
    // Explicit index ordering makes cent ties stable, independent of the sorting algorithm.
    remainders.sort_unstable_by(|(left, left_index), (right, right_index)| {
        right.cmp(left).then_with(|| left_index.cmp(right_index))
    });
    let remaining = (i128::from(total) - allocated) as usize;
    for &(_, index) in remainders.iter().take(remaining) {
        amounts[index] += 1;
    }
    Ok(amounts)
}

pub(crate) fn per_trade_amount(monthly: i64, trading_days: u32) -> Result<i64, String> {
    if monthly < 0 {
        return Err("月度投资金额不能小于 0".into());
    }
    if !(1..=31).contains(&trading_days) {
        return Err("预计交易日数必须位于 1 到 31 之间".into());
    }
    Ok(monthly / i64::from(trading_days))
}

pub(crate) fn average_income(monthly_totals: &[i64]) -> Result<Option<i64>, String> {
    if monthly_totals.iter().any(|&total| total < 0) {
        return Err("月收入合计不能小于 0".into());
    }
    if !monthly_totals.iter().any(|&total| total > 0) {
        return Ok(None);
    }
    let sum: i128 = monthly_totals.iter().map(|&total| i128::from(total)).sum();
    let count = monthly_totals.len() as i128;
    let average = (sum + count / 2) / count;
    if average == 0 {
        return Ok(None);
    }
    i64::try_from(average)
        .map(Some)
        .map_err(|_| "平均月收入超出范围".to_string())
}

pub(crate) fn complete_months(
    today: NaiveDate,
    count: u32,
) -> Result<Vec<(NaiveDate, NaiveDate)>, String> {
    if !(1..=12).contains(&count) {
        return Err("收入均值月份数必须位于 1 到 12 之间".into());
    }
    let mut end = today
        .with_day(1)
        .ok_or_else(|| "月份日期超出范围".to_string())?;
    let mut months = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let start = end
            .checked_sub_months(Months::new(1))
            .ok_or_else(|| "月份日期超出范围".to_string())?;
        months.push((start, end));
        end = start;
    }
    months.reverse();
    Ok(months)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn config() -> Config {
        default_config("CNY".into(), date(2024, 1, 1))
    }

    #[test]
    fn complete_months_excludes_current_month_on_first_and_last_days() {
        let expected = vec![
            (date(2023, 12, 1), date(2024, 1, 1)),
            (date(2024, 1, 1), date(2024, 2, 1)),
            (date(2024, 2, 1), date(2024, 3, 1)),
        ];
        assert_eq!(complete_months(date(2024, 3, 1), 3).unwrap(), expected);
        assert_eq!(complete_months(date(2024, 3, 31), 3).unwrap(), expected);
        assert_eq!((expected[2].1 - expected[2].0).num_days(), 29);
        let ordinary = complete_months(date(2023, 3, 31), 1).unwrap();
        assert_eq!((ordinary[0].1 - ordinary[0].0).num_days(), 28);
    }

    #[test]
    fn complete_months_crosses_years_and_checks_supported_date_range() {
        let months = complete_months(date(2025, 1, 15), 12).unwrap();
        assert_eq!(months.len(), 12);
        assert_eq!(months.first(), Some(&(date(2024, 1, 1), date(2024, 2, 1))));
        assert_eq!(months.last(), Some(&(date(2024, 12, 1), date(2025, 1, 1))));
        assert!(months.windows(2).all(|pair| pair[0].1 == pair[1].0));
        assert!(complete_months(date(2025, 1, 1), 0).is_err());
        assert!(complete_months(date(2025, 1, 1), 13).is_err());
        assert!(complete_months(NaiveDate::MIN, 1).is_err());
        let latest = complete_months(NaiveDate::MAX, 1).unwrap();
        assert_eq!(latest[0].1, NaiveDate::MAX.with_day(1).unwrap());
    }

    #[test]
    fn income_average_includes_zero_months_and_rounds_half_up() {
        assert_eq!(average_income(&[12_000, 0, 0]).unwrap(), Some(4_000));
        assert_eq!(average_income(&[0, 1]).unwrap(), Some(1));
        assert_eq!(average_income(&[1, 0, 0]).unwrap(), None);
        assert_eq!(average_income(&[1, 1, 0]).unwrap(), Some(1));
        assert_eq!(average_income(&[100, 101]).unwrap(), Some(101));
        assert_eq!(average_income(&[i64::MAX; 12]).unwrap(), Some(i64::MAX));
        assert!(average_income(&[1, -1]).is_err());
    }

    #[test]
    fn no_inferred_income_requests_manual_fallback_instead_of_zero() {
        for totals in [&[][..], &[0][..], &[0, 0, 0][..], &[1, 0, 0][..]] {
            assert_eq!(average_income(totals).unwrap(), None);
        }
        let mut plan = config();
        plan.manual_monthly_income = " 1234.56 ".into();
        assert!(validate_config(&plan).is_ok());
        assert_eq!(parse_money(&plan.manual_monthly_income).unwrap(), 123_456);
        plan.manual_monthly_income = " ".into();
        assert!(validate_config(&plan).is_ok());
        for invalid in ["0", "-1", "0.001", "NaN"] {
            plan.manual_monthly_income = invalid.into();
            assert!(validate_config(&plan).is_err());
        }
    }

    #[test]
    fn money_parsing_preserves_cents_and_rejects_precision_and_overflow() {
        assert_eq!(parse_money("0.01").unwrap(), 1);
        assert_eq!(parse_money("12.3").unwrap(), 1_230);
        assert_eq!(parse_money(" 12.34 ").unwrap(), 1_234);
        assert_eq!(parse_money("92233720368547758.07").unwrap(), i64::MAX);
        for invalid in [
            "",
            "0",
            "-0.01",
            "1.001",
            "1.000",
            "NaN",
            "inf",
            "1e2",
            "1_000",
            "1,000",
            "1.2.3",
            ".",
            "92233720368547758.08",
            "99999999999999999999999999999",
        ] {
            assert!(parse_money(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn budget_uses_floor_cents_and_wide_multiplication() {
        assert_eq!(monthly_budget(101, 5_000).unwrap(), 50);
        assert_eq!(monthly_budget(1, 9_999).unwrap(), 0);
        assert_eq!(monthly_budget(0, 10_000).unwrap(), 0);
        assert_eq!(monthly_budget(i64::MAX, 10_000).unwrap(), i64::MAX);
        assert_eq!(monthly_budget(i64::MAX, 0).unwrap(), 0);
        assert_eq!(monthly_budget(i64::MAX, 5_000).unwrap(), i64::MAX / 2);
        assert!(monthly_budget(-1, 1_000).is_err());
        assert!(monthly_budget(1, -1).is_err());
        assert!(monthly_budget(1, 10_001).is_err());
    }

    #[test]
    fn allocation_distributes_remainder_by_weight_then_input_order() {
        assert_eq!(allocate_monthly(1, &[5_000, 5_000]).unwrap(), vec![1, 0]);
        assert_eq!(allocate_monthly(2, &[2_500; 4]).unwrap(), vec![1, 1, 0, 0]);
        assert_eq!(allocate_monthly(3, &[2_500; 4]).unwrap(), vec![1, 1, 1, 0]);
        assert_eq!(
            allocate_monthly(1, &[3_333, 3_333, 3_334]).unwrap(),
            vec![0, 0, 1]
        );
        assert_eq!(
            allocate_monthly(101, &[5_000, 0, 5_000]).unwrap(),
            vec![51, 0, 50]
        );
        assert_eq!(allocate_monthly(0, &[0, 10_000]).unwrap(), vec![0, 0]);
        assert_eq!(
            allocate_monthly(i64::MAX, &[10_000, 0]).unwrap(),
            vec![i64::MAX, 0]
        );
    }

    #[test]
    fn allocation_preserves_exact_totals_and_never_funds_zero_weights() {
        let portfolios: &[&[i64]] = &[
            &[5_000, 5_000],
            &[0, 3_333, 3_333, 3_334],
            &[1, 9_998, 1],
            &[500; 20],
        ];
        for shares in portfolios {
            for total in (0..=101).chain([10_001, i64::MAX]) {
                let amounts = allocate_monthly(total, shares).unwrap();
                assert_eq!(
                    amounts.iter().map(|&v| i128::from(v)).sum::<i128>(),
                    i128::from(total)
                );
                for (&amount, &share) in amounts.iter().zip(*shares) {
                    let floor = (i128::from(total) * i128::from(share) / 10_000) as i64;
                    assert!(amount == floor || i128::from(amount) == i128::from(floor) + 1);
                    if share == 0 {
                        assert_eq!(amount, 0);
                    }
                }
            }
        }
        for shares in [
            &[][..],
            &[-1, 10_001][..],
            &[10_001][..],
            &[9_999][..],
            &[i64::MAX, i64::MAX][..],
        ] {
            assert!(allocate_monthly(1, shares).is_err());
        }
        assert!(allocate_monthly(-1, &[10_000]).is_err());
    }

    #[test]
    fn each_trade_floors_without_exceeding_monthly_target() {
        assert_eq!(per_trade_amount(101, 20).unwrap(), 5);
        assert_eq!(per_trade_amount(19, 20).unwrap(), 0);
        assert_eq!(per_trade_amount(20, 20).unwrap(), 1);
        assert_eq!(per_trade_amount(0, 1).unwrap(), 0);
        assert_eq!(per_trade_amount(i64::MAX, 1).unwrap(), i64::MAX);
        for days in 1..=31 {
            for total in [0, 1, 19, 20, 101, i64::MAX] {
                let daily = per_trade_amount(total, days).unwrap();
                let estimated = i128::from(daily) * i128::from(days);
                assert!(estimated <= i128::from(total));
                assert!(i128::from(total) - estimated < i128::from(days));
            }
        }
        assert!(per_trade_amount(-1, 20).is_err());
        assert!(per_trade_amount(1, 0).is_err());
        assert!(per_trade_amount(1, 32).is_err());
    }

    #[test]
    fn validation_enforces_scalar_bounds_and_positive_account_ids() {
        let mut plan = config();
        plan.income_months = 1;
        plan.investment_bps = 0;
        plan.emergency_months = 0;
        plan.trading_days = 1;
        plan.from_account_id = Some(1);
        plan.parent_account_id = Some(i64::MAX);
        plan.manual_monthly_income = "92233720368547758.07".into();
        assert!(validate_config(&plan).is_ok());
        plan.income_months = 12;
        plan.investment_bps = 10_000;
        plan.emergency_months = 60;
        plan.trading_days = 31;
        assert!(validate_config(&plan).is_ok());
        let invalid_changes: &[fn(&mut Config)] = &[
            |p| p.currency = "invalid".into(),
            |p| p.name = " ".into(),
            |p| p.name = "名".repeat(201),
            |p| p.income_months = 0,
            |p| p.income_months = 13,
            |p| p.investment_bps = -1,
            |p| p.investment_bps = 10_001,
            |p| p.emergency_months = 61,
            |p| p.trading_days = 0,
            |p| p.trading_days = 32,
            |p| p.from_account_id = Some(0),
            |p| p.parent_account_id = Some(-1),
            |p| p.reserve_account_ids = vec![0],
            |p| p.reserve_account_ids = vec![-1],
            |p| p.reserve_account_ids = vec![1, 1],
            |p| p.reserve_account_ids = (1..=21).collect(),
        ];
        for change in invalid_changes {
            let mut invalid = plan.clone();
            change(&mut invalid);
            assert!(validate_config(&invalid).is_err());
        }
        plan.name = "名".repeat(200);
        plan.reserve_account_ids = (1..=20).collect();
        assert!(validate_config(&plan).is_ok());
    }

    #[test]
    fn validation_checks_allocation_identity_weights_binding_and_strategy() {
        let plan = config();
        let invalid_changes: &[fn(&mut Config)] = &[
            |p| p.allocations.clear(),
            |p| p.allocations = vec![p.allocations[0].clone(); 21],
            |p| p.allocations[0].key = " ".into(),
            |p| p.allocations[0].key = "中文".into(),
            |p| p.allocations[0].key = "x".repeat(81),
            |p| p.allocations[1].key = p.allocations[0].key.clone(),
            |p| p.allocations[0].name = " ".into(),
            |p| p.allocations[0].name = "名".repeat(201),
            |p| p.allocations[0].asset_class = "stock".into(),
            |p| p.allocations[0].share_bps = -1,
            |p| p.allocations[0].share_bps = 10_001,
            |p| p.allocations[0].share_bps = 4_999,
            |p| p.allocations[0].fund_account_id = Some(0),
            |p| p.allocations[0].fund_account_id = Some(-1),
            |p| {
                p.allocations[0].fund_account_id = Some(1);
                p.allocations[1].fund_account_id = Some(1);
            },
            |p| p.allocations[0].strategy = "unknown".into(),
            |p| p.allocations[0].fee_rate_bps = -1,
            |p| p.allocations[0].fee_rate_bps = 10_001,
            |p| p.allocations[0].strategy = "smart".into(),
            |p| {
                p.allocations[0].strategy = "smart".into();
                p.allocations[0].index_code = "H30455".into();
                p.allocations[0].moving_average_days = 181;
            },
        ];
        for change in invalid_changes {
            let mut invalid = plan.clone();
            change(&mut invalid);
            assert!(validate_config(&invalid).is_err());
        }
        for class in ["equity", "bond", "gold", "cash", "other"] {
            let mut valid = plan.clone();
            valid.allocations[0].asset_class = class.into();
            valid.allocations[0].key = "x".repeat(80);
            valid.allocations[0].name = "名".repeat(200);
            valid.allocations[0].share_bps = 0;
            valid.allocations[1].share_bps = 10_000;
            valid.allocations[0].fund_account_id = Some(i64::MAX);
            valid.allocations[0].fee_rate_bps = 10_000;
            assert!(validate_config(&valid).is_ok());
        }
        for days in [120, 180, 250, 500] {
            let mut valid = plan.clone();
            valid.allocations[0].strategy = "smart".into();
            valid.allocations[0].index_code = "H30455".into();
            valid.allocations[0].moving_average_days = days;
            assert!(validate_config(&valid).is_ok());
        }
        let mut twenty = plan;
        let row = twenty.allocations[0].clone();
        twenty.allocations = (0..20)
            .map(|i| Allocation {
                key: format!("row-{i}"),
                share_bps: 500,
                ..row.clone()
            })
            .collect();
        assert!(validate_config(&twenty).is_ok());
    }
}
