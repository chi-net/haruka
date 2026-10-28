use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "federated_requests")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 双方共享的请求 UUID，明文。
    #[sea_orm(unique)]
    pub federation_id: String,
    pub instance_id: i64,
    /// "local"（本方创建）/ "remote"（对方创建镜像）。
    pub origin: String,
    /// 本方视角：borrow（我借入）/ lend（我借出）。
    pub kind: String,
    /// 金额整数分，使用配对密钥加密（实例锁定时也可接收）。
    pub amount: String,
    pub currency: String,
    /// 备注，配对密钥加密。
    pub note: String,
    /// "open" / "confirmed" / "settled" / "revoked"。
    pub status: String,
    /// 本地入账后的 debt_requests.id（可空）。
    pub local_debt_request_id: Option<i64>,
    /// 本地入账用账户与借贷对象（本方）。
    pub account_id: Option<i64>,
    pub person_id: Option<i64>,
    /// 对方视角状态缓存（明文状态标签）。
    pub remote_status: String,
    pub remote_status_at: Option<DateTimeUtc>,
    pub last_reminded_at: Option<DateTimeUtc>,
    /// 最近一次推送失败原因（空表示同步正常）。
    pub sync_error: String,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
