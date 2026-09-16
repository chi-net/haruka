use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "debt_requests")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 可选的既有借贷对象；0 信息仍以公开提交的加密快照为准。
    pub person_id: Option<i64>,
    /// 收款账户 ID 明文保存，账户名称和收款标识只存在于加密快照中。
    pub account_id: i64,
    /// "borrow" | "repayment_received"
    pub kind: String,
    #[sea_orm(unique)]
    pub token_hash: Vec<u8>,
    /// URL 令牌使用 DEK 加密，供已解锁用户复制链接和读取提交。
    pub token: String,
    pub label: String,
    pub salt: Vec<u8>,
    /// URL 令牌派生密钥加密的不可变请求快照。
    pub snapshot: String,
    /// 打款人提交的信息，使用同一请求密钥加密；未提交时为空。
    pub submission: String,
    /// 创建、提交、确认三个阶段组成的追加式 SHA-256 验证链。
    pub request_digest: Vec<u8>,
    pub submission_digest: Vec<u8>,
    pub confirmation_digest: Vec<u8>,
    /// "open" | "submitted" | "confirmed" | "revoked"
    pub status: String,
    /// 无时区 SQLite DateTime，按 UTC 解释。
    pub expires_at: DateTime,
    pub submitted_at: Option<DateTimeUtc>,
    pub confirmed_at: Option<DateTimeUtc>,
    pub confirmed_debt_record_id: Option<i64>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
