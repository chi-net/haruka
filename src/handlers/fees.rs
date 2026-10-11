use axum::http::StatusCode;
use rust_decimal::{prelude::ToPrimitive, Decimal, RoundingStrategy};
use std::str::FromStr;

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

pub(crate) fn parse_rate(value: &str) -> HandlerResult<i64> {
    if value.trim().is_empty() {
        return Ok(0);
    }
    let rate = Decimal::from_str(value.trim()).map_err(|_| bad_request("手续费率格式不正确"))?;
    if rate < Decimal::ZERO || rate > Decimal::from(100) {
        return Err(bad_request("手续费率必须位于 0% 到 100% 之间"));
    }
    let bps = rate * Decimal::from(100);
    if !bps.fract().is_zero() {
        return Err(bad_request("手续费率最多保留两位小数"));
    }
    bps.to_i64().ok_or_else(|| bad_request("手续费率超出范围"))
}

pub(crate) fn format_rate(bps: i64) -> String {
    Decimal::new(bps, 2).normalize().to_string()
}

pub(crate) fn calculate(amount: i64, fee_rate_bps: i64) -> HandlerResult<i64> {
    if amount < 0 || !(0..=10_000).contains(&fee_rate_bps) {
        return Err(bad_request("手续费金额或费率无效"));
    }
    let numerator = i128::from(amount) * i128::from(fee_rate_bps);
    let rounded = (numerator + 5_000) / 10_000;
    i64::try_from(rounded).map_err(|_| bad_request("手续费计算超出范围"))
}

pub(crate) fn resolve(
    amount: i64,
    default_rate: i64,
    rate_input: &str,
    amount_input: &str,
) -> HandlerResult<(i64, i64)> {
    let rate = if rate_input.trim().is_empty() {
        default_rate
    } else {
        parse_rate(rate_input)?
    };
    if amount < 0 || !(0..=10_000).contains(&rate) {
        return Err(bad_request("手续费金额或费率无效"));
    }
    let fee = if amount_input.trim().is_empty() {
        calculate(amount, rate)?
    } else {
        let decimal = Decimal::from_str(amount_input.trim())
            .map_err(|_| bad_request("手续费金额格式不正确"))?;
        if decimal < Decimal::ZERO {
            return Err(bad_request("手续费金额不能为负数"));
        }
        decimal
            .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
            .checked_mul(Decimal::from(100))
            .and_then(|value| value.to_i64())
            .ok_or_else(|| bad_request("手续费金额超出范围"))?
    };
    Ok((rate, fee))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rate_and_explicit_overrides_are_independent() {
        assert_eq!(resolve(100_000, 10, "", "").unwrap(), (10, 100));
        assert_eq!(resolve(100_000, 10, "0.2", "").unwrap(), (20, 200));
        assert_eq!(resolve(100_000, 10, "0", "").unwrap(), (0, 0));
        assert_eq!(resolve(100_000, 10, "", "0").unwrap(), (10, 0));
        assert_eq!(resolve(100_000, 10, "0.2", "1.23").unwrap(), (20, 123));
    }

    #[test]
    fn calculated_fees_round_half_up_without_float_or_overflow() {
        assert_eq!(calculate(499, 10).unwrap(), 0);
        assert_eq!(calculate(500, 10).unwrap(), 1);
        assert_eq!(calculate(i64::MAX, 10_000).unwrap(), i64::MAX);
        assert_eq!(resolve(1, 0, "0", "0.005").unwrap(), (0, 1));
    }

    #[test]
    fn rejects_negative_out_of_range_and_overprecise_values() {
        for rate in ["-0.001", "100.001", "0.001", "not-a-rate"] {
            assert!(parse_rate(rate).is_err(), "accepted rate {rate}");
        }
        for fee in ["-0.001", "92233720368547758.08", "not-a-fee"] {
            assert!(resolve(100, 10, "", fee).is_err(), "accepted fee {fee}");
        }
        assert!(calculate(100, -1).is_err());
        assert!(calculate(100, 10_001).is_err());
        assert_eq!(parse_rate("0.1000").unwrap(), 10);
    }
}
