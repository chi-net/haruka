use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "bill_shares")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 仅用于管理来源；源账单删除后分享快照仍保留。
    pub source_bill_id: i64,
    #[sea_orm(unique)]
    pub token_hash: Vec<u8>,
    /// URL 令牌使用 DEK 加密，供已解锁用户重新复制链接。
    pub token: String,
    /// 已解锁管理页面使用的简短说明，使用 DEK 加密。
    pub label: String,
    pub salt: Vec<u8>,
    /// 使用 URL 令牌和可选密码派生的密钥加密的不可变账单快照。
    pub snapshot: String,
    pub has_password: bool,
    /// 无时区 SQLite DateTime，按 UTC 解释。
    pub expires_at: DateTime,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
