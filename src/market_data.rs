use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sea_orm::{
    sea_query::OnConflict, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};
use serde::Deserialize;

use crate::{entity::market_index_quote, AppState};

pub const MOVING_AVERAGE_OPTIONS: &[i32] = &[120, 180, 250, 500];

#[derive(Clone, Copy)]
pub struct IndexOption {
    pub code: &'static str,
    pub name: &'static str,
    pub source: &'static str,
}

pub const INDEX_OPTIONS: &[IndexOption] = &[
    IndexOption {
        code: "000300",
        name: "沪深300",
        source: "中证指数官网",
    },
    IndexOption {
        code: "H30455",
        name: "中证沪港深500",
        source: "中证指数官网",
    },
    IndexOption {
        code: "HSI",
        name: "恒生指数",
        source: "东方财富公开行情",
    },
];

pub fn index_option(code: &str) -> Option<IndexOption> {
    INDEX_OPTIONS.iter().copied().find(|item| item.code == code)
}

pub fn valid_moving_average(days: i32) -> bool {
    MOVING_AVERAGE_OPTIONS.contains(&days)
}

fn quote_id(index_code: &str, date: NaiveDate) -> String {
    format!("{index_code}:{date}")
}

#[derive(Deserialize)]
struct CsIndexResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Vec<CsIndexQuote>,
}

#[derive(Deserialize)]
struct CsIndexQuote {
    #[serde(rename = "tradeDate")]
    trade_date: String,
    close: serde_json::Value,
}

#[derive(Deserialize)]
struct EastMoneyResponse {
    data: Option<EastMoneyData>,
}

#[derive(Deserialize)]
struct EastMoneyData {
    #[serde(default)]
    klines: Vec<String>,
}

struct QuoteValue {
    date: NaiveDate,
    close: Decimal,
}

async fn fetch_csindex(
    state: &AppState,
    index_code: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<QuoteValue>, String> {
    let response = state
        .market_client
        .get("https://www.csindex.com.cn/csindex-home/perf/index-perf")
        .query(&[
            ("indexCode", index_code.to_string()),
            ("startDate", start.format("%Y%m%d").to_string()),
            ("endDate", end.format("%Y%m%d").to_string()),
        ])
        .send()
        .await
        .map_err(|error| format!("中证指数行情请求失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("中证指数行情返回 HTTP {}", response.status()));
    }
    let payload = response
        .json::<CsIndexResponse>()
        .await
        .map_err(|error| format!("中证指数行情格式无效：{error}"))?;
    if !payload.success {
        return Err(format!("中证指数行情请求失败：{}", payload.msg));
    }
    payload
        .data
        .into_iter()
        .map(|item| {
            let date = NaiveDate::parse_from_str(&item.trade_date, "%Y%m%d")
                .map_err(|_| "中证指数行情日期格式无效".to_string())?;
            let close = Decimal::from_str(item.close.to_string().trim_matches('"'))
                .map_err(|_| "中证指数收盘点位格式无效".to_string())?;
            if close <= Decimal::ZERO {
                return Err("中证指数收盘点位必须大于 0".into());
            }
            Ok(QuoteValue { date, close })
        })
        .collect()
}

async fn fetch_hang_seng(
    state: &AppState,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<QuoteValue>, String> {
    let response = state
        .market_client
        .get("https://push2his.eastmoney.com/api/qt/stock/kline/get")
        .query(&[
            ("secid", "100.HSI".to_string()),
            ("klt", "101".to_string()),
            ("fqt", "0".to_string()),
            ("lmt", "100000".to_string()),
            ("beg", start.format("%Y%m%d").to_string()),
            ("end", end.format("%Y%m%d").to_string()),
            ("fields1", "f1,f2,f3,f4,f5,f6".to_string()),
            (
                "fields2",
                "f51,f52,f53,f54,f55,f56,f57,f58,f59,f60,f61".to_string(),
            ),
        ])
        .send()
        .await
        .map_err(|error| format!("恒生指数行情请求失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("恒生指数行情返回 HTTP {}", response.status()));
    }
    let payload = response
        .json::<EastMoneyResponse>()
        .await
        .map_err(|error| format!("恒生指数行情格式无效：{error}"))?;
    let data = payload
        .data
        .ok_or_else(|| "恒生指数行情没有返回数据".to_string())?;
    data.klines
        .into_iter()
        .map(|line| {
            let fields = line.split(',').collect::<Vec<_>>();
            if fields.len() < 3 {
                return Err("恒生指数日线字段不完整".to_string());
            }
            let date = NaiveDate::parse_from_str(fields[0], "%Y-%m-%d")
                .map_err(|_| "恒生指数行情日期格式无效".to_string())?;
            let close =
                Decimal::from_str(fields[2]).map_err(|_| "恒生指数收盘点位格式无效".to_string())?;
            if close <= Decimal::ZERO {
                return Err("恒生指数收盘点位必须大于 0".into());
            }
            Ok(QuoteValue { date, close })
        })
        .collect()
}

pub async fn refresh_index(
    state: &AppState,
    index_code: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<usize, String> {
    let option = index_option(index_code).ok_or_else(|| "不支持的跟踪指数".to_string())?;
    if start > end {
        return Err("指数行情查询区间无效".into());
    }
    let _guard = state.market_fetches.lock().await;
    let values = if index_code == "HSI" {
        fetch_hang_seng(state, start, end).await?
    } else {
        fetch_csindex(state, index_code, start, end).await?
    };
    if values.is_empty() {
        return Err(format!("{}没有返回可用的指数日线", option.source));
    }
    let fetched_at = chrono::Utc::now();
    let count = values.len();
    for value in values {
        market_index_quote::Entity::insert(market_index_quote::ActiveModel {
            id: Set(quote_id(index_code, value.date)),
            index_code: Set(index_code.to_string()),
            trade_date: Set(value.date),
            close: Set(value.close.normalize().to_string()),
            source: Set(option.source.to_string()),
            fetched_at: Set(fetched_at),
        })
        .on_conflict(
            OnConflict::column(market_index_quote::Column::Id)
                .update_columns([
                    market_index_quote::Column::Close,
                    market_index_quote::Column::Source,
                    market_index_quote::Column::FetchedAt,
                ])
                .to_owned(),
        )
        .exec(&state.db)
        .await
        .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

pub struct SmartDecision {
    pub index_code: String,
    pub index_name: String,
    pub moving_average_days: i32,
    pub quote_date: NaiveDate,
    pub close: Decimal,
    pub moving_average: Decimal,
    pub deviation_percent: Decimal,
    pub multiplier_bps: i64,
    pub source: String,
}

fn multiplier_for_deviation(deviation_percent: Decimal) -> i64 {
    let distance = deviation_percent.abs();
    let step = if distance <= Decimal::from(2) {
        0
    } else if distance <= Decimal::from(3) {
        1
    } else if distance <= Decimal::from(4) {
        2
    } else if distance <= Decimal::from(5) {
        3
    } else if distance <= Decimal::from(6) {
        4
    } else {
        5
    };
    if deviation_percent > Decimal::ZERO {
        10_000 - step * 1_000
    } else {
        10_000 + step * 1_000
    }
}

pub async fn smart_decision(
    state: &AppState,
    index_code: &str,
    moving_average_days: i32,
    trade_date: NaiveDate,
) -> Result<SmartDecision, String> {
    let option = index_option(index_code).ok_or_else(|| "不支持的跟踪指数".to_string())?;
    if !valid_moving_average(moving_average_days) {
        return Err("不支持的均线周期".into());
    }
    let quotes = market_index_quote::Entity::find()
        .filter(market_index_quote::Column::IndexCode.eq(index_code))
        .filter(market_index_quote::Column::TradeDate.lt(trade_date))
        .order_by_desc(market_index_quote::Column::TradeDate)
        .limit(moving_average_days as u64)
        .all(&state.db)
        .await
        .map_err(|error| error.to_string())?;
    if quotes.len() != moving_average_days as usize {
        return Err(format!(
            "{}的{}日均线数据不足：需要 {} 个交易日，当前只有 {} 个",
            option.name,
            moving_average_days,
            moving_average_days,
            quotes.len()
        ));
    }
    let parsed = quotes
        .iter()
        .map(|quote| Decimal::from_str(&quote.close).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let moving_average =
        parsed.iter().copied().sum::<Decimal>() / Decimal::from(moving_average_days);
    if moving_average <= Decimal::ZERO {
        return Err("指数均线必须大于 0".into());
    }
    let close = parsed[0];
    let deviation_percent = (close - moving_average) * Decimal::from(100) / moving_average;
    let multiplier_bps = multiplier_for_deviation(deviation_percent);
    Ok(SmartDecision {
        index_code: index_code.to_string(),
        index_name: option.name.to_string(),
        moving_average_days,
        quote_date: quotes[0].trade_date,
        close,
        moving_average,
        deviation_percent,
        multiplier_bps,
        source: quotes[0].source.clone(),
    })
}

pub fn adjusted_amount(base_amount: i64, multiplier_bps: i64) -> Result<i64, String> {
    let numerator = i128::from(base_amount)
        .checked_mul(i128::from(multiplier_bps))
        .and_then(|value| value.checked_add(5_000))
        .ok_or_else(|| "聪明定投金额计算超出范围".to_string())?;
    i64::try_from(numerator / 10_000).map_err(|_| "聪明定投金额计算超出范围".to_string())
}

pub fn format_point(value: Decimal) -> String {
    value.round_dp(2).normalize().to_string()
}

pub fn format_percent(value: Decimal) -> String {
    let value = value.round_dp(2).normalize();
    if value > Decimal::ZERO {
        format!("+{value}%")
    } else {
        format!("{value}%")
    }
}

pub fn format_multiplier(multiplier_bps: i64) -> String {
    format!("{}%", Decimal::new(multiplier_bps, 2).normalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    #[test]
    fn smart_multiplier_respects_every_boundary() {
        for (deviation, expected) in [
            ("0", 10_000),
            ("2", 10_000),
            ("2.01", 9_000),
            ("3", 9_000),
            ("3.01", 8_000),
            ("4", 8_000),
            ("4.01", 7_000),
            ("5", 7_000),
            ("5.01", 6_000),
            ("6", 6_000),
            ("6.01", 5_000),
            ("-2", 10_000),
            ("-2.01", 11_000),
            ("-3", 11_000),
            ("-3.01", 12_000),
            ("-4", 12_000),
            ("-4.01", 13_000),
            ("-5", 13_000),
            ("-5.01", 14_000),
            ("-6", 14_000),
            ("-6.01", 15_000),
        ] {
            assert_eq!(multiplier_for_deviation(decimal(deviation)), expected);
        }
    }

    #[test]
    fn adjusted_amount_rounds_to_nearest_cent() {
        assert_eq!(adjusted_amount(10_001, 9_000).unwrap(), 9_001);
        assert_eq!(adjusted_amount(10_001, 11_000).unwrap(), 11_001);
    }
}
