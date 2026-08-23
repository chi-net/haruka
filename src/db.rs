use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Schema, Statement};

use crate::entity::{
    account, account_detail, bill, category, debt_person, debt_record, meta, passkey, recovery,
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
    let mut create_accounts = schema.create_table_from_entity(account::Entity);
    db.execute(builder.build(create_accounts.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "accounts", "kind", "TEXT NOT NULL DEFAULT 'other'").await;
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
    let mut create_bills = schema.create_table_from_entity(bill::Entity);
    db.execute(builder.build(create_bills.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "bills", "is_food", "BOOLEAN NOT NULL DEFAULT 0").await;
    let mut create_transfers = schema.create_table_from_entity(transfer::Entity);
    db.execute(builder.build(create_transfers.if_not_exists()))
        .await
        .expect("建表失败");
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
