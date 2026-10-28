use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "linked_instances")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 对方实例规范化 URL（scheme://host[:port]），明文。
    pub domain: String,
    /// 我方对外域名（配对时确定），明文。
    pub self_domain: String,
    /// 绑定的本地借贷对象（debt_people.id），配对时按对方域名建立。
    pub person_id: Option<i64>,
    /// 对方角色：creditor（对方是我的债权人）/ debtor（对方是我的借款人）。
    pub role: String,
    /// "pending"（等待对方验证）/ "paired"（已绑定）。
    pub status: String,
    /// 配对密钥，明文存储：实例锁定期间仍需接收并解密联邦消息。
    #[sea_orm(column_type = "Blob")]
    pub pairing_key: Vec<u8>,
    /// 一次性配对码 SHA-256 哈希（pending 期间索引；配对成功清空）。
    #[sea_orm(column_type = "Blob")]
    pub pairing_code_hash: Vec<u8>,
    pub pairing_code_expires_at: Option<DateTime>,
    pub paired_at: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
