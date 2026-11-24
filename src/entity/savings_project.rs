use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "savings_projects")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 攒钱项目名（密文）
    pub name: String,
    /// 只进不出的附属攒钱账户
    #[sea_orm(unique)]
    pub account_id: i64,
    /// 自动转入的资金来源账户；空表示未启用自动转入
    pub source_account_id: Option<i64>,
    /// 每次自动转入金额（整数分密文）
    pub amount: String,
    /// "day" | "week" | "month"
    pub cycle: String,
    pub next_run_at: Option<DateTime>,
    pub auto_enabled: bool,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
