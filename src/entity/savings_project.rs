use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "savings_projects")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 攒钱项目名（密文）
    pub name: String,
    /// "item"（实体物品）或 "fund"（资金目标）
    pub kind: String,
    /// 资金目标金额（整数分密文）；实体物品为空
    #[sea_orm(default_value = "")]
    pub target_amount: String,
    /// 只进不出的附属攒钱账户
    #[sea_orm(unique)]
    pub account_id: i64,
    /// 已划入攒钱资金池的金额（整数分密文），与账户可用余额独立。
    #[sea_orm(default_value = "")]
    pub saved_amount: String,
    /// 自动转入的资金来源账户；空表示未启用自动转入
    pub source_account_id: Option<i64>,
    /// 每次自动转入金额（整数分密文）
    pub amount: String,
    /// "day" | "week" | "month"
    pub cycle: String,
    pub next_run_at: Option<DateTime>,
    pub auto_enabled: bool,
    /// "active" | "realized"
    #[sea_orm(default_value = "active")]
    pub status: String,
    /// 实际兑现/消费金额（整数分密文）；资金目标达成时为 0
    #[sea_orm(default_value = "")]
    pub realized_amount: String,
    pub realized_at: Option<DateTimeUtc>,
    /// 兑现说明（密文）
    #[sea_orm(default_value = "")]
    pub realized_note: String,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
