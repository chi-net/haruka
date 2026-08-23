use sea_orm::{ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait, PaginatorTrait, Schema, Set};

use crate::entity::{account, bill};

pub async fn init() -> DatabaseConnection {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://haruka.db?mode=rwc".to_string());
    let db = Database::connect(&url).await.expect("数据库连接失败");

    let builder = db.get_database_backend();
    let schema = Schema::new(DbBackend::Sqlite);
    let mut create_accounts = schema.create_table_from_entity(account::Entity);
    db.execute(builder.build(create_accounts.if_not_exists())).await.expect("建表失败");
    let mut create_bills = schema.create_table_from_entity(bill::Entity);
    db.execute(builder.build(create_bills.if_not_exists())).await.expect("建表失败");

    if account::Entity::find().count(&db).await.expect("查询失败") == 0 {
        account::ActiveModel {
            name: Set("默认账户".to_string()),
            note: Set(String::new()),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("创建默认账户失败");
    }

    db
}
