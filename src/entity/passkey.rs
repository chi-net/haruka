use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "passkeys")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub credential_id: Vec<u8>,
    /// webauthn-rs 的 Passkey JSON（只含凭据 ID、公钥与计数器）。
    pub credential: String,
    /// 名称属于用户数据，使用 DEK 加密。
    pub name: String,
    pub dek_nonce: Vec<u8>,
    pub wrapped_dek: Vec<u8>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
