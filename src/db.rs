use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Schema};

use crate::entity::{account, bill, meta, recovery};

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
    let mut create_accounts = schema.create_table_from_entity(account::Entity);
    db.execute(builder.build(create_accounts.if_not_exists()))
        .await
        .expect("建表失败");
    let mut create_bills = schema.create_table_from_entity(bill::Entity);
    db.execute(builder.build(create_bills.if_not_exists()))
        .await
        .expect("建表失败");

    db
}
