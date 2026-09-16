use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "investment_sms_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// ISO 时间和原始短信的 SHA-256，用于避免同一短信被重复处理。
    pub event_hash: String,
    pub occurred_at: DateTimeUtc,
    /// success、failure、bank_transfer 或 quick_payment。
    pub kind: String,
    /// executed、failed、pending、confirmed、duplicate、unmatched 或 error。
    pub status: String,
    pub plan_id: Option<i64>,
    /// 确认本人转账或信用卡还款后生成的转账流水。
    pub transfer_id: Option<i64>,
    /// 原始短信密文。
    pub raw: String,
    /// 面向用户的解析或处理结果密文。
    pub detail: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
