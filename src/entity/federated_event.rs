use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "federated_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub federation_id: String,
    pub seq: i64,
    /// "created" / "confirmed" / "settled" / "reminded"。
    pub kind: String,
    /// 配对密钥加密的事件负载密文（与线路上一致，双方各自重算验证）。
    pub payload: String,
    /// 链式摘要：digest(域分隔 || federation_id || seq || kind || 前序摘要 || payload)。
    #[sea_orm(column_type = "Blob")]
    pub digest: Vec<u8>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
