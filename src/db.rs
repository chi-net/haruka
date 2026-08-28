use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Schema, Statement};

use crate::entity::{
    account, account_detail, balance_adjustment, bill, category, debt_person, debt_record,
    exchange_rate, installment_item, installment_plan, meta, passkey, preference, recovery,
    subscription, transfer,
};

pub async fn init() -> DatabaseConnection {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://haruka.db?mode=rwc".to_string());
    let db = Database::connect(&url).await.expect("数据库连接失败");

    let builder = db.get_database_backend();
    let schema = Schema::new(DbBackend::Sqlite);
    let mut create_meta = schema.create_table_from_entity(meta::Entity);
    db.execute(builder.build(create_meta.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_recovery = schema.create_table_from_entity(recovery::Entity);
    db.execute(builder.build(create_recovery.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_passkeys = schema.create_table_from_entity(passkey::Entity);
    db.execute(builder.build(create_passkeys.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(
        &db,
        "passkeys",
        "dek_wrappers",
        "TEXT NOT NULL DEFAULT '[]'",
    )
    .await;
    let mut create_preferences = schema.create_table_from_entity(preference::Entity);
    db.execute(builder.build(create_preferences.if_not_exists()))
        .await
        .expect("建表失败");
    db.execute(Statement::from_string(
        DbBackend::Sqlite,
        "INSERT OR IGNORE INTO preferences (id, default_currency) VALUES (1, 'CNY')".to_string(),
    ))
    .await
    .expect("初始化默认货币失败");
    let mut create_exchange_rates = schema.create_table_from_entity(exchange_rate::Entity);
    db.execute(builder.build(create_exchange_rates.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_accounts = schema.create_table_from_entity(account::Entity);
    db.execute(builder.build(create_accounts.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "accounts", "kind", "TEXT NOT NULL DEFAULT 'other'").await;
    ensure_column(&db, "accounts", "currency", "TEXT NOT NULL DEFAULT 'CNY'").await;
    ensure_column(
        &db,
        "accounts",
        "balance_offset",
        "TEXT NOT NULL DEFAULT ''",
    )
    .await;
    let mut create_account_details = schema.create_table_from_entity(account_detail::Entity);
    db.execute(builder.build(create_account_details.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(
        &db,
        "account_details",
        "credit_limit",
        "TEXT NOT NULL DEFAULT ''",
    )
    .await;
    ensure_column(
        &db,
        "account_details",
        "billing_day",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await;
    let mut create_balance_adjustments =
        schema.create_table_from_entity(balance_adjustment::Entity);
    db.execute(builder.build(create_balance_adjustments.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_bills = schema.create_table_from_entity(bill::Entity);
    db.execute(builder.build(create_bills.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "bills", "is_food", "BOOLEAN NOT NULL DEFAULT 0").await;
    let mut create_installment_plans = schema.create_table_from_entity(installment_plan::Entity);
    db.execute(builder.build(create_installment_plans.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_installment_items = schema.create_table_from_entity(installment_item::Entity);
    db.execute(builder.build(create_installment_items.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "installment_items", "repayment_account_id", "INTEGER").await;
    ensure_column(&db, "installment_items", "principal_transfer_id", "INTEGER").await;
    ensure_column(&db, "installment_items", "charge_bill_id", "INTEGER").await;
    let mut create_transfers = schema.create_table_from_entity(transfer::Entity);
    db.execute(builder.build(create_transfers.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "transfers", "to_amount", "TEXT NOT NULL DEFAULT ''").await;
    let mut create_debt_people = schema.create_table_from_entity(debt_person::Entity);
    db.execute(builder.build(create_debt_people.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_debt_records = schema.create_table_from_entity(debt_record::Entity);
    db.execute(builder.build(create_debt_records.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_categories = schema.create_table_from_entity(category::Entity);
    db.execute(builder.build(create_categories.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "categories", "is_food", "BOOLEAN NOT NULL DEFAULT 0").await;
    let mut create_subscriptions = schema.create_table_from_entity(subscription::Entity);
    db.execute(builder.build(create_subscriptions.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(
        &db,
        "subscriptions",
        "period",
        "TEXT NOT NULL DEFAULT 'month'",
    )
    .await;
    ensure_column(
        &db,
        "subscriptions",
        "currency",
        "TEXT NOT NULL DEFAULT 'CNY'",
    )
    .await;

    db
}

async fn ensure_column(
    db: &DatabaseConnection,
    table: &'static str,
    column: &'static str,
    definition: &'static str,
) {
    let rows = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA table_info({table})"),
        ))
        .await
        .expect("读取表结构失败");
    let exists = rows.iter().any(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|name| name == column)
    });
    if !exists {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        ))
        .await
        .unwrap_or_else(|error| panic!("升级 {table}.{column} 失败: {error}"));
    }
}
