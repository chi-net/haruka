use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "investment_sms_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 时间、发件号码和原始短信的 SHA-256，用于避免同一短信被重复处理。
    pub event_hash: String,
    pub occurred_at: DateTimeUtc,
    /// 内置短信类型或用户模板 action。
    pub kind: String,
    /// executed、failed、pending、confirmed、ignored、duplicate、unmatched、audit 或 error。
    pub status: String,
    pub plan_id: Option<i64>,
    /// 确认本人转账或信用卡还款后生成的转账流水。
    pub transfer_id: Option<i64>,
    /// 用户明确启用普通收支动作后生成的账单。
    pub bill_id: Option<i64>,
    /// 发件号码密文，旧记录默认为空。
    pub sender: String,
    /// MatchedSms 解析快照 JSON 密文，内置及旧记录默认为空。
    pub parsed: String,
    /// 原始短信密文。
    pub raw: String,
    /// 面向用户的解析或处理结果密文。
    pub detail: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
