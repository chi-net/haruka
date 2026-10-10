use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::{Datelike, Duration, Months, NaiveDate};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, IntoActiveModel, QueryFilter,
    Set, TransactionTrait,
};

use crate::{
    crypto,
    entity::{bill, category},
    handlers::ClientTimeZone,
    AppState,
};

pub(crate) struct CountStatus {
    pub id: i64,
    pub name: String,
    pub period_label: String,
    pub period_range: String,
    pub limit: u32,
    pub used: u64,
    pub remaining: u64,
    pub over_by: u64,
    pub percent: String,
    pub bar_percent: u32,
    pub near_limit: bool,
    pub at_limit: bool,
    pub over_limit: bool,
}

pub(crate) fn parse_limit(kind: &str, period: &str, raw: &str) -> Result<Option<u32>, String> {
    if kind == "income" {
        return Ok(None);
    }
    if kind != "expense" {
        return Err("分类类型无效".into());
    }
    if !matches!(period, "day" | "week" | "month") {
        return Err("次数限制周期必须为每日、每周或每月".into());
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("次数限制必须为正整数".into());
    }
    raw.parse::<u32>()
        .ok()
        .filter(|limit| *limit > 0)
        .map(Some)
        .ok_or_else(|| "次数限制必须在 1 到 4294967295 之间".into())
}

pub(crate) fn decrypt_limit(
    dek: &crypto::Dek,
    category: &category::Model,
) -> Result<Option<u32>, String> {
    if category.kind == "income" || category.count_limit.is_empty() {
        return Ok(None);
    }
    let raw = crypto::decrypt(dek, &category.count_limit)
        .and_then(|value| String::from_utf8(value).ok())
        .ok_or_else(|| format!("分类 {} 的次数限制无法解密", category.id))?;
    parse_limit(&category.kind, &category.count_limit_period, &raw)?
        .map(Some)
        .ok_or_else(|| format!("分类 {} 的次数限制为空或无效", category.id))
}

fn period_bounds(
    period: &str,
    today: NaiveDate,
) -> Result<(NaiveDate, NaiveDate, &'static str), String> {
    match period {
        "day" => Ok((today, today, "本日")),
        "week" => {
            let start = today
                .checked_sub_signed(Duration::days(
                    today.weekday().num_days_from_monday().into(),
                ))
                .ok_or_else(|| "本周起始日期超出范围".to_string())?;
            let end = start
                .checked_add_signed(Duration::days(6))
                .ok_or_else(|| "本周结束日期超出范围".to_string())?;
            Ok((start, end, "本周"))
        }
        "month" => {
            let start = today
                .with_day(1)
                .ok_or_else(|| "本月日期无效".to_string())?;
            let end = start
                .checked_add_months(Months::new(1))
                .and_then(|next| next.pred_opt())
                .ok_or_else(|| "本月结束日期超出范围".to_string())?;
            Ok((start, end, "本月"))
        }
        _ => Err("次数限制周期无效".into()),
    }
}

pub(crate) fn statuses<'a>(
    dek: &crypto::Dek,
    zone: ClientTimeZone,
    today: NaiveDate,
    categories: &[category::Model],
    bills: impl IntoIterator<Item = &'a bill::Model>,
) -> Result<Vec<CountStatus>, String> {
    let mut result = Vec::new();
    let mut indices = HashMap::new();
    for category in categories {
        if category.kind != "expense" {
            continue;
        }
        let Some(limit) = decrypt_limit(dek, category)? else {
            continue;
        };
        let (start, end, label) = period_bounds(&category.count_limit_period, today)?;
        let name = crypto::decrypt(dek, &category.name)
            .and_then(|value| String::from_utf8(value).ok())
            .ok_or_else(|| format!("分类 {} 的名称无法解密", category.id))?;
        indices.insert(category.id, (result.len(), start));
        result.push(CountStatus {
            id: category.id,
            name,
            period_label: label.into(),
            period_range: if start == end {
                start.format("%Y-%m-%d").to_string()
            } else {
                format!("{} — {}", start.format("%Y-%m-%d"), end.format("%Y-%m-%d"))
            },
            limit,
            used: 0,
            remaining: u64::from(limit),
            over_by: 0,
            percent: String::new(),
            bar_percent: 0,
            near_limit: false,
            at_limit: false,
            over_limit: false,
        });
    }
    if result.is_empty() {
        return Ok(result);
    }
    for bill in bills {
        if bill.kind != "expense" {
            continue;
        }
        let Some(id) = bill.category_id.filter(|id| *id > 0) else {
            continue;
        };
        let Some(&(index, start)) = indices.get(&id) else {
            continue;
        };
        let date = zone.date(bill.happened_at);
        if date >= start && date <= today {
            result[index].used += 1;
        }
    }
    for status in &mut result {
        let limit = u64::from(status.limit);
        status.remaining = limit.saturating_sub(status.used);
        status.over_by = status.used.saturating_sub(limit);
        let percent = u128::from(status.used) * 100 / u128::from(limit);
        status.percent = percent.to_string();
        status.bar_percent = percent.min(100) as u32;
        status.near_limit =
            u128::from(status.used) * 5 >= u128::from(limit) * 4 || status.remaining <= 1;
        status.at_limit = status.used == limit;
        status.over_limit = status.used > limit;
    }
    Ok(result)
}

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub(crate) async fn backfill_category_ids(
    state: &AppState,
    dek: &crypto::Dek,
) -> Result<(), (StatusCode, String)> {
    let _guard = state.balance_writes.lock().await;
    let transaction = state.db.begin().await.map_err(err500)?;
    associate_legacy_ids(&transaction, dek).await?;
    transaction.commit().await.map_err(err500)
}

async fn associate_legacy_ids(
    transaction: &DatabaseTransaction,
    dek: &crypto::Dek,
) -> Result<(), (StatusCode, String)> {
    let legacy = bill::Entity::find()
        .filter(bill::Column::CategoryId.is_null())
        .all(transaction)
        .await
        .map_err(err500)?;
    if legacy.is_empty() {
        return Ok(());
    }
    let categories = category::Entity::find()
        .all(transaction)
        .await
        .map_err(err500)?;
    let mut identities = HashMap::<&str, HashMap<String, i64>>::new();
    for category in &categories {
        let name = crypto::decrypt(dek, &category.name)
            .and_then(|value| String::from_utf8(value).ok())
            .ok_or_else(|| err500(format!("分类 {} 的名称无法解密", category.id)))?;
        identities
            .entry(&category.kind)
            .or_default()
            .insert(name, category.id);
    }
    for bill in legacy {
        let name = crypto::decrypt(dek, &bill.category)
            .and_then(|value| String::from_utf8(value).ok())
            .ok_or_else(|| err500(format!("账单 {} 的分类名称无法解密", bill.id)))?;
        let id = identities
            .get(bill.kind.as_str())
            .and_then(|names| names.get(&name))
            .copied()
            .unwrap_or(0);
        let mut active = bill.into_active_model();
        active.category_id = Set(Some(id));
        active.update(transaction).await.map_err(err500)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::account;
    use chrono::NaiveDateTime;
    use sea_orm::{ConnectOptions, ConnectionTrait, Database, Schema};

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn time(value: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn category(dek: &crypto::Dek, id: i64, period: &str, limit: Option<u32>) -> category::Model {
        category::Model {
            id,
            kind: "expense".into(),
            name: crypto::encrypt(dek, "餐饮".as_bytes()),
            is_food: true,
            count_limit: limit
                .map(|limit| crypto::encrypt(dek, limit.to_string().as_bytes()))
                .unwrap_or_default(),
            count_limit_period: period.into(),
            created_at: time("2026-01-01 00:00:00").and_utc(),
        }
    }

    fn bill(id: Option<i64>, happened_at: &str) -> bill::Model {
        bill::Model {
            id: 1,
            account_id: 1,
            kind: "expense".into(),
            amount: "not needed for count".into(),
            category: "not needed for count".into(),
            category_id: id,
            is_food: true,
            note: String::new(),
            happened_at: time(happened_at),
            created_at: time("2026-01-01 00:00:00").and_utc(),
        }
    }

    #[test]
    fn limits_require_positive_ascii_integers_and_valid_expense_periods() {
        for period in ["day", "week", "month"] {
            assert_eq!(parse_limit("expense", period, " "), Ok(None));
            assert_eq!(parse_limit("expense", period, " 001 "), Ok(Some(1)));
            assert_eq!(
                parse_limit("expense", period, "4294967295"),
                Ok(Some(u32::MAX))
            );
        }
        for raw in [
            "0",
            "000",
            "-1",
            "+1",
            "1.0",
            "1e2",
            "1 2",
            "１",
            "4294967296",
        ] {
            assert!(parse_limit("expense", "month", raw).is_err(), "{raw}");
        }
        assert!(parse_limit("expense", "year", "").is_err());
        assert!(parse_limit("transfer", "month", "1").is_err());
        assert_eq!(parse_limit("income", "invalid", "invalid"), Ok(None));
    }

    #[test]
    fn enabled_corrupt_limits_fail_instead_of_disappearing() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let mut item = category(&dek, 1, "month", Some(10));
        assert_eq!(decrypt_limit(&dek, &item), Ok(Some(10)));
        for raw in ["", "0", "1.5", "4294967296"] {
            item.count_limit = crypto::encrypt(&dek, raw.as_bytes());
            assert!(decrypt_limit(&dek, &item).is_err());
        }
        item.count_limit = "invalid ciphertext".into();
        assert!(decrypt_limit(&dek, &item).is_err());
        item.count_limit = crypto::encrypt(&dek, &[0xff]);
        assert!(decrypt_limit(&dek, &item).is_err());
        item.count_limit.clear();
        assert_eq!(decrypt_limit(&dek, &item), Ok(None));
        item.kind = "income".into();
        item.count_limit = "invalid ciphertext".into();
        assert_eq!(decrypt_limit(&dek, &item), Ok(None));
    }

    #[test]
    fn counts_respect_local_periods_today_and_stable_identity() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let zone = ClientTimeZone(chrono_tz::Asia::Shanghai);
        let mut income_category = category(&dek, 5, "month", Some(20));
        income_category.kind = "income".into();
        let categories = [
            category(&dek, 1, "day", Some(20)),
            category(&dek, 2, "week", Some(20)),
            category(&dek, 3, "month", Some(20)),
            category(&dek, 4, "month", None),
            income_category,
        ];
        let mut bills = Vec::new();
        // UTC 16:00 is the following local day. Monday is the first day of the week.
        for id in 1..=3 {
            for happened_at in [
                "2026-02-28 15:59:59",
                "2026-02-28 16:00:00",
                "2026-03-01 15:59:59",
                "2026-03-01 16:00:00",
                "2026-03-02 15:59:59",
                "2026-03-02 16:00:00",
            ] {
                bills.push(bill(Some(id), happened_at));
            }
        }
        let mut income = bill(Some(3), "2026-03-02 00:00:00");
        income.kind = "income".into();
        let mut transfer = bill(Some(3), "2026-03-02 00:00:00");
        transfer.kind = "transfer".into();
        bills.extend([
            income,
            transfer,
            bill(None, "2026-03-02 00:00:00"),
            bill(Some(0), "2026-03-02 00:00:00"),
            bill(Some(99), "2026-03-02 00:00:00"),
        ]);
        let rows = statuses(&dek, zone, date("2026-03-02"), &categories, bills.iter()).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.used).collect::<Vec<_>>(),
            [2, 2, 4]
        );
        assert_eq!(rows[0].period_range, "2026-03-02");
        assert_eq!(rows[1].period_range, "2026-03-02 — 2026-03-08");
        assert_eq!(rows[2].period_range, "2026-03-01 — 2026-03-31");
        assert_eq!(rows[1].period_label, "本周");
    }

    #[test]
    fn daylight_saving_and_cross_year_weeks_use_local_calendar_dates() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let categories = [category(&dek, 1, "day", Some(10))];
        let bills = [
            bill(Some(1), "2026-03-08 04:59:59"),
            bill(Some(1), "2026-03-08 05:00:00"),
            bill(Some(1), "2026-03-09 03:59:59"),
            bill(Some(1), "2026-03-09 04:00:00"),
        ];
        let rows = statuses(
            &dek,
            ClientTimeZone(chrono_tz::America::New_York),
            date("2026-03-08"),
            &categories,
            bills.iter(),
        )
        .unwrap();
        assert_eq!(rows[0].used, 2);
        assert_eq!(
            period_bounds("week", date("2027-01-01")).unwrap(),
            (date("2026-12-28"), date("2027-01-03"), "本周")
        );
        assert_eq!(
            period_bounds("month", date("2028-02-29")).unwrap(),
            (date("2028-02-01"), date("2028-02-29"), "本月")
        );
    }

    #[test]
    fn warning_thresholds_include_small_limits_and_exact_eighty_percent() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        for (limit, used, near, at, over, remaining, percent) in [
            (1, 0, true, false, false, 1, "0"),
            (2, 0, false, false, false, 2, "0"),
            (2, 1, true, false, false, 1, "50"),
            (3, 2, true, false, false, 1, "66"),
            (5, 3, false, false, false, 2, "60"),
            (5, 4, true, false, false, 1, "80"),
            (10, 7, false, false, false, 3, "70"),
            (10, 8, true, false, false, 2, "80"),
            (10, 10, true, true, false, 0, "100"),
            (10, 11, true, false, true, 0, "110"),
            (
                u32::MAX,
                1,
                false,
                false,
                false,
                u64::from(u32::MAX) - 1,
                "0",
            ),
        ] {
            let categories = [category(&dek, 1, "month", Some(limit))];
            let bills = (0..used)
                .map(|_| bill(Some(1), "2026-10-10 00:00:00"))
                .collect::<Vec<_>>();
            let row = statuses(
                &dek,
                ClientTimeZone(chrono_tz::UTC),
                date("2026-10-10"),
                &categories,
                bills.iter(),
            )
            .unwrap()
            .remove(0);
            assert_eq!(row.used, used);
            assert_eq!(
                (row.near_limit, row.at_limit, row.over_limit),
                (near, at, over)
            );
            assert_eq!(row.remaining, remaining);
            assert_eq!(row.over_by, used.saturating_sub(u64::from(limit)));
            assert_eq!(row.percent, percent);
            assert_eq!(row.bar_percent, percent.parse::<u32>().unwrap().min(100));
        }
    }

    #[test]
    fn renames_keep_counts_and_recreated_names_never_adopt_history() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let zone = ClientTimeZone(chrono_tz::UTC);
        let bills = [bill(Some(1), "2026-10-10 00:00:00")];
        let mut renamed = category(&dek, 1, "month", Some(2));
        renamed.name = crypto::encrypt(&dek, "新名称".as_bytes());
        let rows = statuses(&dek, zone, date("2026-10-10"), &[renamed], bills.iter()).unwrap();
        assert_eq!(rows[0].name, "新名称");
        assert_eq!(rows[0].used, 1);
        assert!(statuses(&dek, zone, date("2026-10-10"), &[], bills.iter())
            .unwrap()
            .is_empty());
        let recreated = [category(&dek, 2, "month", Some(2))];
        assert_eq!(
            statuses(&dek, zone, date("2026-10-10"), &recreated, bills.iter()).unwrap()[0].used,
            0
        );
    }

    #[tokio::test]
    async fn legacy_association_is_kind_specific_and_permanent_without_changing_snapshots() {
        let mut options = ConnectOptions::new("sqlite::memory:");
        options.max_connections(1);
        let db = Database::connect(options).await.unwrap();
        let backend = db.get_database_backend();
        let schema = Schema::new(backend);
        for table in [
            schema.create_table_from_entity(account::Entity),
            schema.create_table_from_entity(category::Entity),
            schema.create_table_from_entity(bill::Entity),
        ] {
            db.execute(backend.build(&table)).await.unwrap();
        }
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        account::ActiveModel {
            id: Set(1),
            name: Set(crypto::encrypt(&dek, "测试账户".as_bytes())),
            kind: Set("other".into()),
            currency: Set("CNY".into()),
            balance_offset: Set(crypto::encrypt_cents(&dek, 0)),
            note: Set(String::new()),
            sms_names: Set(String::new()),
            created_at: Set(time("2026-01-01 00:00:00").and_utc()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let expense = category(&dek, 1, "month", Some(3))
            .into_active_model()
            .insert(&db)
            .await
            .unwrap();
        let mut income = category(&dek, 2, "month", None);
        income.kind = "income".into();
        income.into_active_model().insert(&db).await.unwrap();
        let mut originals = Vec::new();
        for (id, kind, name, identity) in [
            (1, "expense", "餐饮", None),
            (2, "income", "餐饮", None),
            (3, "expense", "已删除", None),
            (4, "expense", "餐饮", Some(99)),
            (5, "expense", "餐饮", Some(0)),
        ] {
            let mut item = bill(identity, "2026-10-10 00:00:00");
            item.id = id;
            item.kind = kind.into();
            item.category = crypto::encrypt(&dek, name.as_bytes());
            item.amount = crypto::encrypt_cents(&dek, 12345);
            item.note = crypto::encrypt(&dek, "旧备注".as_bytes());
            originals.push(item.clone());
            item.into_active_model().insert(&db).await.unwrap();
        }
        let transaction = db.begin().await.unwrap();
        associate_legacy_ids(&transaction, &dek).await.unwrap();
        transaction.commit().await.unwrap();
        for (original, identity) in originals.iter().zip([1, 2, 0, 99, 0]) {
            let actual = bill::Entity::find_by_id(original.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            let mut expected = original.clone();
            expected.category_id = Some(identity);
            assert_eq!(actual, expected);
        }
        category::Entity::delete_by_id(expense.id)
            .exec(&db)
            .await
            .unwrap();
        let mut recreated = category(&dek, 3, "month", Some(3));
        recreated.name = crypto::encrypt(&dek, "已删除".as_bytes());
        let mut active = recreated.into_active_model();
        active.id = sea_orm::ActiveValue::NotSet;
        let recreated = active.insert(&db).await.unwrap();
        assert!(recreated.id > expense.id);
        let mut active = category(&dek, 4, "month", Some(3)).into_active_model();
        active.id = sea_orm::ActiveValue::NotSet;
        let same_name = active.insert(&db).await.unwrap();
        assert!(same_name.id > expense.id);
        let transaction = db.begin().await.unwrap();
        associate_legacy_ids(&transaction, &dek).await.unwrap();
        transaction.commit().await.unwrap();
        for (id, identity) in [(1, 1), (3, 0), (4, 99), (5, 0)] {
            assert_eq!(
                bill::Entity::find_by_id(id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap()
                    .category_id,
                Some(identity)
            );
        }
    }
}
