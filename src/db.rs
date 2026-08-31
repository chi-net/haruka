use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Schema, Statement};

use crate::entity::{
    account, account_detail, balance_adjustment, bill, category, debt_person, debt_record,
    exchange_rate, installment_item, installment_plan, investment_execution, market_closed_day,
    meta, passkey, preference, recovery, recurring_investment, subscription, transfer,
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
    let mut create_recurring_investments =
        schema.create_table_from_entity(recurring_investment::Entity);
    db.execute(builder.build(create_recurring_investments.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(
        &db,
        "recurring_investments",
        "fee_rate_bps",
        "TEXT NOT NULL DEFAULT ''",
    )
    .await;
    let mut create_investment_executions =
        schema.create_table_from_entity(investment_execution::Entity);
    db.execute(builder.build(create_investment_executions.if_not_exists()))
        .await
        .expect("建表失败");
    ensure_column(&db, "investment_executions", "fee_bill_id", "INTEGER").await;
    db.execute(Statement::from_string(
        DbBackend::Sqlite,
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_investment_execution_plan_date ON investment_executions (plan_id, trade_date)".to_string(),
    ))
    .await
    .expect("创建定投执行唯一索引失败");
    let mut create_market_closed_days = schema.create_table_from_entity(market_closed_day::Entity);
    db.execute(builder.build(create_market_closed_days.if_not_exists()))
        .await
        .expect("建表失败");
    seed_market_closed_days(&db).await;
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

async fn seed_market_closed_days(db: &DatabaseConnection) {
    // 上海证券交易所公布的 2025、2026 年休市安排。周末也列入部分公告范围，
    // 但交易日判断本身会先排除周六和周日。
    const CLOSED_DAYS: &[(&str, &str)] = &[
        ("2025-01-01", "元旦"),
        ("2025-01-28", "春节"),
        ("2025-01-29", "春节"),
        ("2025-01-30", "春节"),
        ("2025-01-31", "春节"),
        ("2025-02-03", "春节"),
        ("2025-02-04", "春节"),
        ("2025-04-04", "清明节"),
        ("2025-05-01", "劳动节"),
        ("2025-05-02", "劳动节"),
        ("2025-05-05", "劳动节"),
        ("2025-06-02", "端午节"),
        ("2025-10-01", "国庆节、中秋节"),
        ("2025-10-02", "国庆节、中秋节"),
        ("2025-10-03", "国庆节、中秋节"),
        ("2025-10-06", "国庆节、中秋节"),
        ("2025-10-07", "国庆节、中秋节"),
        ("2025-10-08", "国庆节、中秋节"),
        ("2026-01-01", "元旦"),
        ("2026-01-02", "元旦"),
        ("2026-02-16", "春节"),
        ("2026-02-17", "春节"),
        ("2026-02-18", "春节"),
        ("2026-02-19", "春节"),
        ("2026-02-20", "春节"),
        ("2026-02-23", "春节"),
        ("2026-04-06", "清明节"),
        ("2026-05-01", "劳动节"),
        ("2026-05-04", "劳动节"),
        ("2026-05-05", "劳动节"),
        ("2026-06-19", "端午节"),
        ("2026-09-25", "中秋节"),
        ("2026-10-01", "国庆节"),
        ("2026-10-02", "国庆节"),
        ("2026-10-05", "国庆节"),
        ("2026-10-06", "国庆节"),
        ("2026-10-07", "国庆节"),
    ];
    for (date, name) in CLOSED_DAYS {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "INSERT OR IGNORE INTO market_closed_days (date, name, source, created_at) VALUES ('{date}', '{name}', 'builtin', CURRENT_TIMESTAMP)"
            ),
        ))
        .await
        .expect("初始化中国大陆市场休市日失败");
    }
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
