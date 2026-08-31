use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "recurring_investments")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 定投计划名（密文）。
    pub name: String,
    pub from_account_id: i64,
    /// 固定接收资金的基金账户，必须是 investment 类型。
    pub fund_account_id: i64,
    /// 每个交易日从扣款账户转出的整数分（密文）。
    pub amount: String,
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
