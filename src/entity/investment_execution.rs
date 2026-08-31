use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "investment_executions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub plan_id: i64,
    /// 中国大陆市场交易日（纯日期，不做时区转换）。
    pub trade_date: Date,
    pub transfer_id: Option<i64>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::recurring_investment::Entity",
        from = "Column::PlanId",
        to = "super::recurring_investment::Column::Id",
        on_delete = "Cascade"
    )]
    Plan,
    #[sea_orm(
        belongs_to = "super::transfer::Entity",
        from = "Column::TransferId",
        to = "super::transfer::Column::Id",
        on_delete = "SetNull"
    )]
    Transfer,
}

impl Related<super::recurring_investment::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Plan.def()
    }
}

impl Related<super::transfer::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Transfer.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
