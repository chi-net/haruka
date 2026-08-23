use std::{collections::HashMap, str::FromStr};

use chrono::{Duration, NaiveDate};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{sea_query::OnConflict, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};
use serde::Deserialize;

use crate::{
    entity::{exchange_rate, preference},
    AppState,
};

pub const FALLBACK_CURRENCY: &str = "CNY";

#[derive(Clone, Copy)]
pub struct CurrencyOption {
    pub code: &'static str,
    pub name: &'static str,
}

pub const CURRENCIES: &[CurrencyOption] = &[
    CurrencyOption {
        code: "CNY",
        name: "人民币",
    },
    CurrencyOption {
        code: "USD",
        name: "美元",
    },
    CurrencyOption {
        code: "EUR",
        name: "欧元",
    },
    CurrencyOption {
        code: "HKD",
        name: "港币",
    },
    CurrencyOption {
        code: "JPY",
        name: "日元",
    },
    CurrencyOption {
        code: "GBP",
        name: "英镑",
    },
    CurrencyOption {
        code: "SGD",
        name: "新加坡元",
    },
    CurrencyOption {
        code: "AUD",
        name: "澳大利亚元",
    },
    CurrencyOption {
        code: "CAD",
        name: "加拿大元",
    },
    CurrencyOption {
        code: "CHF",
        name: "瑞士法郎",
    },
    CurrencyOption {
        code: "KRW",
        name: "韩元",
    },
    CurrencyOption {
        code: "TWD",
        name: "新台币",
    },
    CurrencyOption {
        code: "NZD",
        name: "新西兰元",
    },
    CurrencyOption {
        code: "THB",
        name: "泰铢",
    },
    CurrencyOption {
        code: "MYR",
        name: "马来西亚林吉特",
    },
    CurrencyOption {
        code: "PHP",
        name: "菲律宾比索",
    },
    CurrencyOption {
        code: "IDR",
        name: "印度尼西亚卢比",
    },
    CurrencyOption {
        code: "INR",
        name: "印度卢比",
    },
    CurrencyOption {
        code: "VND",
        name: "越南盾",
    },
    CurrencyOption {
        code: "AED",
        name: "阿联酋迪拉姆",
    },
    CurrencyOption {
        code: "TRY",
        name: "土耳其里拉",
    },
    CurrencyOption {
        code: "RUB",
        name: "俄罗斯卢布",
    },
];

pub fn valid(code: &str) -> bool {
    CURRENCIES.iter().any(|currency| currency.code == code)
}

pub async fn default_currency(state: &AppState) -> Result<String, String> {
    Ok(preference::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(|error| error.to_string())?
        .map(|preference| preference.default_currency)
        .unwrap_or_else(|| FALLBACK_CURRENCY.to_string()))
}

pub fn format(cents: i64, currency: &str) -> String {
    format!("{} {}", currency, rust_decimal::Decimal::new(cents, 2))
}

#[derive(Deserialize)]
struct RateResponse {
    date: NaiveDate,
    rate: serde_json::Value,
}

fn cache_id(date: NaiveDate, base: &str, quote: &str) -> String {
    format!("{date}:{base}:{quote}")
}

pub struct RateInfo {
    pub rate: Decimal,
    pub rate_date: NaiveDate,
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn cached_rate(
    state: &AppState,
    date: NaiveDate,
    base: &str,
    quote: &str,
) -> Result<Option<RateInfo>, String> {
    let row = exchange_rate::Entity::find_by_id(cache_id(date, base, quote))
        .one(&state.db)
        .await
        .map_err(|error| error.to_string())?;
    row.map(|row| {
        Decimal::from_str(&row.rate)
            .map(|rate| RateInfo {
                rate,
                rate_date: row.rate_date,
                fetched_at: Some(row.fetched_at),
            })
            .map_err(|error| error.to_string())
    })
    .transpose()
}

fn cache_is_fresh(
    requested_date: NaiveDate,
    rate_date: NaiveDate,
    fetched_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    rate_date >= requested_date
        || requested_date < chrono::Local::now().date_naive()
        || chrono::Utc::now() - fetched_at < Duration::hours(6)
}

async fn latest_cached_rate(
    state: &AppState,
    date: NaiveDate,
    base: &str,
    quote: &str,
) -> Result<Option<RateInfo>, String> {
    let row = exchange_rate::Entity::find()
        .filter(exchange_rate::Column::BaseCurrency.eq(base))
        .filter(exchange_rate::Column::QuoteCurrency.eq(quote))
        .filter(exchange_rate::Column::RequestedDate.lte(date))
        .order_by_desc(exchange_rate::Column::RequestedDate)
        .one(&state.db)
        .await
        .map_err(|error| error.to_string())?;
    row.map(|row| {
        Decimal::from_str(&row.rate)
            .map(|rate| RateInfo {
                rate,
                rate_date: row.rate_date,
                fetched_at: Some(row.fetched_at),
            })
            .map_err(|error| error.to_string())
    })
    .transpose()
}

pub async fn rate_with_info(
    state: &AppState,
    base: &str,
    quote: &str,
    requested_date: NaiveDate,
) -> Result<RateInfo, String> {
    if base == quote {
        return Ok(RateInfo {
            rate: Decimal::ONE,
            rate_date: requested_date,
            fetched_at: None,
        });
    }
    if !valid(base) || !valid(quote) {
        return Err(format!("不支持的货币：{base}/{quote}"));
    }
    if let Some(info) = cached_rate(state, requested_date, base, quote).await? {
        if cache_is_fresh(
            requested_date,
            info.rate_date,
            info.fetched_at.expect("缓存汇率必须有抓取时间"),
        ) {
            return Ok(info);
        }
    }

    let _fetch_guard = state.fx_fetches.lock().await;
    if let Some(info) = cached_rate(state, requested_date, base, quote).await? {
        if cache_is_fresh(
            requested_date,
            info.rate_date,
            info.fetched_at.expect("缓存汇率必须有抓取时间"),
        ) {
            return Ok(info);
        }
    }

    let mut last_error = String::new();
    for days_back in 0..=7 {
        let date = requested_date - Duration::days(days_back);
        let url = format!("https://api.frankfurter.dev/v2/rate/{base}/{quote}?date={date}");
        match state.fx_client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<RateResponse>().await {
                    Ok(response) => {
                        let value = response.rate.to_string();
                        let parsed = Decimal::from_str(value.trim_matches('"'))
                            .map_err(|error| format!("汇率格式无效：{error}"))?;
                        if parsed <= Decimal::ZERO {
                            return Err("汇率必须大于 0".into());
                        }
                        let fetched_at = chrono::Utc::now();
                        exchange_rate::Entity::insert(exchange_rate::ActiveModel {
                            id: Set(cache_id(requested_date, base, quote)),
                            requested_date: Set(requested_date),
                            rate_date: Set(response.date),
                            base_currency: Set(base.to_string()),
                            quote_currency: Set(quote.to_string()),
                            rate: Set(parsed.to_string()),
                            fetched_at: Set(fetched_at),
                        })
                        .on_conflict(
                            OnConflict::column(exchange_rate::Column::Id)
                                .update_columns([
                                    exchange_rate::Column::RateDate,
                                    exchange_rate::Column::Rate,
                                    exchange_rate::Column::FetchedAt,
                                ])
                                .to_owned(),
                        )
                        .exec(&state.db)
                        .await
                        .map_err(|error| error.to_string())?;
                        return Ok(RateInfo {
                            rate: parsed,
                            rate_date: response.date,
                            fetched_at: Some(fetched_at),
                        });
                    }
                    Err(error) => last_error = error.to_string(),
                }
            }
            Ok(response) => last_error = format!("HTTP {}", response.status()),
            Err(error) => last_error = error.to_string(),
        }
    }

    if let Some(info) = latest_cached_rate(state, requested_date, base, quote).await? {
        return Ok(info);
    }
    Err(format!(
        "无法取得 {requested_date} 的 {base}/{quote} 汇率，且没有可用缓存：{last_error}"
    ))
}

pub async fn rate(
    state: &AppState,
    base: &str,
    quote: &str,
    requested_date: NaiveDate,
) -> Result<Decimal, String> {
    Ok(rate_with_info(state, base, quote, requested_date)
        .await?
        .rate)
}

pub async fn convert_cents(
    state: &AppState,
    cents: i64,
    base: &str,
    quote: &str,
    date: NaiveDate,
) -> Result<i64, String> {
    let converted = Decimal::from(cents) * rate(state, base, quote, date).await?;
    converted
        .round()
        .to_i64()
        .ok_or_else(|| "换算后的金额超出范围".to_string())
}

pub struct RateTable {
    quote: String,
    rates: HashMap<String, Decimal>,
}

impl RateTable {
    pub async fn load<I>(
        state: &AppState,
        currencies: I,
        quote: &str,
        date: NaiveDate,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut rates = HashMap::new();
        rates.insert(quote.to_string(), Decimal::ONE);
        for base in currencies {
            if !rates.contains_key(&base) {
                rates.insert(base.clone(), rate(state, &base, quote, date).await?);
            }
        }
        Ok(Self {
            quote: quote.into(),
            rates,
        })
    }

    pub fn convert(&self, cents: i64, base: &str) -> Result<i64, String> {
        let rate = self
            .rates
            .get(base)
            .ok_or_else(|| format!("缺少 {base}/{} 汇率", self.quote))?;
        (Decimal::from(cents) * *rate)
            .round()
            .to_i64()
            .ok_or_else(|| "换算后的金额超出范围".to_string())
    }
}
