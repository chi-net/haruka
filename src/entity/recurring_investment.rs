use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "recurring_investments")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 定投计划名（密文）。
    pub name: String,
    pub from_account_id: i64,
    /// 固定接收资金的具体基金，必须是 investment_fund 类型。
    pub fund_account_id: i64,
    /// 固定/聪明策略的基准金额，或手动策略的默认金额（密文，可为 0）。
    pub amount: String,
    /// 手续费率基点（1 bp = 0.01%）密文。
    pub fee_rate_bps: String,
    /// fixed、smart、manual 或 sms；旧计划默认为 fixed。
    pub strategy: String,
    /// 聪明定投跟踪的公开指数代码。
    pub index_code: String,
    /// 移动平均线包含的指数交易日数。
    pub moving_average_days: i32,
    pub start_date: Date,
    pub next_trade_date: Date,
    pub active: bool,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::FromAccountId",
        to = "super::account::Column::Id",
        on_delete = "Cascade"
    )]
    FromAccount,
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::FundAccountId",
        to = "super::account::Column::Id",
        on_delete = "Cascade"
    )]
    FundAccount,
    #[sea_orm(has_many = "super::investment_execution::Entity")]
    Executions,
}

impl Related<super::investment_execution::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Executions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
