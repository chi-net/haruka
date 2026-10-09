use crate::{
    crypto,
    entity::{account, balance_adjustment, investment_sms_event},
    handlers::accounts,
    AppState,
};
use axum::http::StatusCode;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder, Set, Statement, TransactionTrait,
};
use std::collections::HashMap;

type HandlerResult<T> = Result<T, (StatusCode, String)>;
fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// Actual monetary/configuration references; metadata and public snapshots deliberately stay on roots.
pub(crate) const ACCOUNT_REFERENCES: &[(&str, &str)] = &[
    ("bills", "account_id"),
    ("debt_records", "account_id"),
    ("transfers", "from_account_id"),
    ("transfers", "to_account_id"),
    ("balance_adjustments", "account_id"),
    ("subscriptions", "auto_debit_account_id"),
    ("recurring_investments", "from_account_id"),
    ("recurring_investments", "fund_account_id"),
    ("installment_plans", "account_id"),
    ("installment_items", "repayment_account_id"),
    ("debt_requests", "account_id"),
    ("debt_requests", "repayment_account_id"),
    ("sms_templates", "account_id"),
];

pub(crate) fn is_money_account(account: &account::Model) -> bool {
    account.kind != "investment"
}

pub(crate) async fn validate_money_account(
    state: &AppState,
    account: &account::Model,
) -> HandlerResult<()> {
    if !is_money_account(account) {
        return Err((
            StatusCode::BAD_REQUEST,
            "投资分组不能直接收付款，请选择具体基金".into(),
        ));
    }
    if account.kind == "investment_fund" {
        let parent_id = account
            .parent_id
            .ok_or_else(|| err500("基金缺少投资分组"))?;
        let parent = account::Entity::find_by_id(parent_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| err500("基金投资分组不存在"))?;
        if parent.kind != "investment"
            || parent.parent_id.is_some()
            || parent.currency != account.currency
        {
            return Err(err500("基金分组或货币不一致"));
        }
    } else if account.parent_id.is_some() {
        return Err(err500("只有基金可以有父账户"));
    }
    Ok(())
}

pub(crate) fn display_names(
    dek: &crypto::Dek,
    accounts: &[account::Model],
) -> HashMap<i64, String> {
    let mut names: HashMap<_, _> = accounts
        .iter()
        .map(|account| (account.id, crypto::decrypt_string(dek, &account.name)))
        .collect();
    for account in accounts
        .iter()
        .filter(|account| account.parent_id.is_some())
    {
        if let Some(parent_name) = account.parent_id.and_then(|id| names.get(&id)) {
            if let Some(name) = names.get(&account.id) {
                let label = format!("{parent_name} / {name}");
                names.insert(account.id, label);
            }
        }
    }
    names
}

pub(crate) fn sms_names(dek: &crypto::Dek, fund: &account::Model) -> HandlerResult<Vec<String>> {
    let mut names = vec![crypto::decrypt_string(dek, &fund.name)];
    if !fund.sms_names.is_empty() {
        let bytes =
            crypto::decrypt(dek, &fund.sms_names).ok_or_else(|| err500("基金短信名称无法解密"))?;
        let aliases: Vec<String> = serde_json::from_slice(&bytes).map_err(err500)?;
        names.extend(aliases);
    }
    Ok(names)
}

pub(crate) fn roll_up_balances(
    accounts: &[account::Model],
    balances: &mut HashMap<i64, i64>,
) -> HandlerResult<()> {
    let by_id: HashMap<_, _> = accounts.iter().map(|a| (a.id, a)).collect();
    for root in accounts.iter().filter(|a| a.kind == "investment") {
        if root.parent_id.is_some() {
            return Err(err500("投资分组不能有父账户"));
        }
        balances.insert(root.id, 0);
    }
    for fund in accounts {
        if fund.kind != "investment_fund" {
            if fund.parent_id.is_some() {
                return Err(err500("只有基金可以有父账户"));
            }
            continue;
        }
        let parent = fund
            .parent_id
            .and_then(|id| by_id.get(&id))
            .ok_or_else(|| err500("基金缺少投资分组"))?;
        if parent.kind != "investment" || parent.currency != fund.currency {
            return Err(err500("基金分组或货币不一致"));
        }
        let value = balances
            .get(&fund.id)
            .copied()
            .ok_or_else(|| err500("基金缺少余额"))?;
        let total = balances
            .get(&parent.id)
            .copied()
            .unwrap_or(0)
            .checked_add(value)
            .ok_or_else(|| err500("持仓总价值超出范围"))?;
        balances.insert(parent.id, total);
    }
    Ok(())
}

pub(crate) async fn ensure_default_funds(state: &AppState, dek: &crypto::Dek) -> HandlerResult<()> {
    state.fund_migration_ready.get_or_try_init(|| async {
        let _guard = state.balance_writes.lock().await;
        let tx = state.db.begin().await.map_err(err500)?;
        let columns = tx.query_all(Statement::from_string(DbBackend::Sqlite, "PRAGMA table_info(recurring_investments)".to_owned())).await.map_err(err500)?;
        let has_old_names = columns.iter().any(|r| r.try_get::<String>("", "name").is_ok_and(|n| n == "sms_fund_name"));
        let roots = account::Entity::find().filter(account::Column::Kind.eq("investment")).all(&tx).await.map_err(err500)?;
        let mut migrated = HashMap::new();
        for root in roots {
            if root.parent_id.is_some() { return Err(err500("投资分组不能有父账户")); }
            let child = account::Entity::find().filter(account::Column::ParentId.eq(root.id)).one(&tx).await.map_err(err500)?;
            if let Some(child) = &child {
                if child.kind != "investment_fund" || child.currency != root.currency { return Err(err500("基金分组或货币不一致")); }
            }
            if child.is_none() {
                let last = balance_adjustment::Entity::find().filter(balance_adjustment::Column::AccountId.eq(root.id)).order_by_desc(balance_adjustment::Column::HappenedAt).one(&tx).await.map_err(err500)?;
                let fund = account::ActiveModel {
                    name: Set(crypto::encrypt(dek, "默认基金".as_bytes())), kind: Set("investment_fund".into()),
                    currency: Set(root.currency.clone()), balance_offset: Set(root.balance_offset.clone()),
                    note: Set(crypto::encrypt(dek, b"")), created_at: Set(root.created_at), parent_id: Set(Some(root.id)),
                    last_calibrated_at: Set(last.map(|a| a.happened_at.and_utc())),
                    sms_names: Set(crypto::encrypt(dek, b"[]")), ..Default::default()
                }.insert(&tx).await.map_err(err500)?;
                let mut active = root.clone().into_active_model();
                active.balance_offset = Set(crypto::encrypt_cents(dek, 0));
                active.update(&tx).await.map_err(err500)?;
                migrated.insert(root.id, fund.id);
            }
        }
        for (&root, &fund) in &migrated {
            for &(table, column) in ACCOUNT_REFERENCES {
                tx.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
                    format!("UPDATE {table} SET {column} = ? WHERE {column} = ?"), [fund.into(), root.into()])).await.map_err(err500)?;
            }
        }
        if has_old_names {
            let rows = tx.query_all(Statement::from_string(DbBackend::Sqlite,
                "SELECT fund_account_id, sms_fund_name FROM recurring_investments WHERE sms_fund_name != ''".to_owned())).await.map_err(err500)?;
            let mut collected: HashMap<i64, Vec<String>> = HashMap::new();
            for row in rows {
                let fund_id: i64 = row.try_get("", "fund_account_id").map_err(err500)?;
                let encoded: String = row.try_get("", "sms_fund_name").map_err(err500)?;
                let bytes = crypto::decrypt(dek, &encoded).ok_or_else(|| err500("旧计划短信基金名称无法解密"))?;
                let name = String::from_utf8(bytes).map_err(err500)?;
                if !name.trim().is_empty() { collected.entry(fund_id).or_default().push(name); }
            }
            for (fund_id, old_names) in collected {
                let fund = account::Entity::find_by_id(fund_id).one(&tx).await.map_err(err500)?
                    .ok_or_else(|| err500("旧计划基金不存在，无法迁移短信名称"))?;
                if fund.kind != "investment_fund" || fund.parent_id.is_none() {
                    return Err(err500("旧计划未关联具体基金，无法安全迁移短信名称"));
                }
                let mut aliases = sms_names(dek, &fund)?;
                aliases.remove(0);
                for name in old_names {
                    if !aliases.contains(&name) { aliases.push(name); }
                }
                let mut active = fund.into_active_model();
                active.sms_names = Set(crypto::encrypt(dek, &serde_json::to_vec(&aliases).map_err(err500)?));
                active.update(&tx).await.map_err(err500)?;
            }
        }
        if !migrated.is_empty() {
            for event in investment_sms_event::Entity::find().all(&tx).await.map_err(err500)? {
                if event.parsed.is_empty() { continue; }
                let bytes = crypto::decrypt(dek, &event.parsed).ok_or_else(|| err500("短信候选快照无法解密"))?;
                let mut snapshot: serde_json::Value = serde_json::from_slice(&bytes).map_err(err500)?;
                if let Some(new_id) = snapshot.get("account_id").and_then(|id| id.as_i64()).and_then(|id| migrated.get(&id)) {
                    snapshot["account_id"] = (*new_id).into();
                    let mut active = event.into_active_model();
                    active.parsed = Set(crypto::encrypt(dek, &serde_json::to_vec(&snapshot).map_err(err500)?));
                    active.update(&tx).await.map_err(err500)?;
                }
            }
        }
        if has_old_names { tx.execute(Statement::from_string(DbBackend::Sqlite, "ALTER TABLE recurring_investments DROP COLUMN sms_fund_name".to_owned())).await.map_err(err500)?; }
        tx.commit().await.map_err(err500)?;
        Ok::<(), (StatusCode, String)>(())
    }).await?;
    Ok(())
}

pub(crate) struct FundReminder {
    pub(crate) id: i64,
    pub(crate) parent_id: i64,
    pub(crate) name: String,
    pub(crate) parent_name: String,
    pub(crate) currency: String,
    pub(crate) amount: i64,
    pub(crate) days_since: i64,
}

pub(crate) async fn due_valuations(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<Vec<FundReminder>> {
    let all = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let now = chrono::Utc::now();
    let mut balances = HashMap::new();
    for fund in all.iter().filter(|a| a.kind == "investment_fund") {
        if (now - fund.last_calibrated_at.unwrap_or(fund.created_at)).num_days() >= 30 {
            balances.insert(
                fund.id,
                accounts::current_balance(state, dek, fund.id).await?,
            );
        }
    }
    valuation_reminders(dek, &all, &balances, now)
}

pub(crate) fn valuation_reminders(
    dek: &crypto::Dek,
    accounts: &[account::Model],
    balances: &HashMap<i64, i64>,
    now: chrono::DateTime<chrono::Utc>,
) -> HandlerResult<Vec<FundReminder>> {
    let by_id: HashMap<_, _> = accounts.iter().map(|a| (a.id, a)).collect();
    let mut reminders = Vec::new();
    for fund in accounts.iter().filter(|a| a.kind == "investment_fund") {
        let days_since = (now - fund.last_calibrated_at.unwrap_or(fund.created_at)).num_days();
        if days_since < 30 {
            continue;
        }
        let amount = balances
            .get(&fund.id)
            .copied()
            .ok_or_else(|| err500("基金缺少余额"))?;
        if amount <= 0 {
            continue;
        }
        let parent = fund
            .parent_id
            .and_then(|id| by_id.get(&id))
            .ok_or_else(|| err500("基金缺少投资分组"))?;
        if parent.kind != "investment"
            || parent.parent_id.is_some()
            || parent.currency != fund.currency
        {
            return Err(err500("基金分组或货币不一致"));
        }
        reminders.push(FundReminder {
            id: fund.id,
            parent_id: parent.id,
            name: crypto::decrypt_string(dek, &fund.name),
            parent_name: crypto::decrypt_string(dek, &parent.name),
            currency: fund.currency.clone(),
            amount,
            days_since,
        });
    }
    reminders.sort_by_key(|r| (std::cmp::Reverse(r.days_since), r.id));
    Ok(reminders)
}

// Refuse deleting even zero-valued funds when any ledger/configuration history remains.
pub(crate) async fn ensure_unused(
    state: &AppState,
    dek: &crypto::Dek,
    id: i64,
) -> HandlerResult<()> {
    if accounts::current_balance(state, dek, id).await? != 0 {
        return Err((StatusCode::BAD_REQUEST, "请先清空基金持仓".into()));
    }
    for &(table, column) in ACCOUNT_REFERENCES {
        let row = state
            .db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("SELECT 1 AS found FROM {table} WHERE {column} = ? LIMIT 1"),
                [id.into()],
            ))
            .await
            .map_err(err500)?;
        if row.is_some() {
            return Err((
                StatusCode::BAD_REQUEST,
                "基金已有流水或配置关联，不能删除；历史必须保留".into(),
            ));
        }
    }
    for event in investment_sms_event::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        if event.parsed.is_empty() {
            continue;
        }
        let bytes =
            crypto::decrypt(dek, &event.parsed).ok_or_else(|| err500("短信候选快照无法解密"))?;
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).map_err(err500)?;
        if snapshot.get("account_id").and_then(|v| v.as_i64()) == Some(id) {
            return Err((
                StatusCode::BAD_REQUEST,
                "基金有关联短信候选，不能删除".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn aggregates_overwrite_root_values_and_validate_hierarchy() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let root = model(&dek, 1, "investment", None);
        let first = model(&dek, 2, "investment_fund", Some(1));
        let second = model(&dek, 3, "investment_fund", Some(1));
        let mut balances = HashMap::from([(1, 999), (2, 120), (3, 80)]);
        roll_up_balances(
            &[root.clone(), first.clone(), second.clone()],
            &mut balances,
        )
        .unwrap();
        assert_eq!(balances[&1], 200);
        assert_eq!(balances[&2], 120);
        let mut wrong_currency = second.clone();
        wrong_currency.currency = "USD".into();
        assert!(roll_up_balances(
            &[root.clone(), first.clone(), wrong_currency],
            &mut balances
        )
        .is_err());
        assert!(roll_up_balances(&[first.clone()], &mut balances).is_err());
        balances.insert(2, i64::MAX);
        balances.insert(3, 1);
        assert!(roll_up_balances(&[root, first, second], &mut balances).is_err());
    }

    #[test]
    fn reminder_threshold_is_thirty_days_and_empty_funds_do_not_nag() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let now = chrono::Utc::now();
        let root = model(&dek, 1, "investment", None);
        let mut first = model(&dek, 2, "investment_fund", Some(1));
        first.created_at = now - chrono::Duration::days(30);
        let mut second = model(&dek, 3, "investment_fund", Some(1));
        second.created_at = now - chrono::Duration::days(60);
        let balances = HashMap::from([(2, 100), (3, 0)]);
        let due = valuation_reminders(
            &dek,
            &[root.clone(), first.clone(), second.clone()],
            &balances,
            now,
        )
        .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, 2);
        assert_eq!(due[0].days_since, 30);
        first.last_calibrated_at = Some(now - chrono::Duration::days(29));
        assert!(
            valuation_reminders(&dek, &[root, first, second], &balances, now)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fund_sms_names_include_canonical_name_and_reject_bad_json() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let mut fund = model(&dek, 2, "investment_fund", Some(1));
        fund.sms_names = crypto::encrypt(&dek, "[\"银行简称\"]".as_bytes());
        assert_eq!(sms_names(&dek, &fund).unwrap(), vec!["账户2", "银行简称"]);
        fund.sms_names = crypto::encrypt(&dek, b"{malformed");
        assert!(sms_names(&dek, &fund).is_err());
    }
}
