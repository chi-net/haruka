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
    pub fee_bill_id: Option<i64>,
    /// 本期计算前的基准金额（密文）。
    pub base_amount: String,
    /// 实际扣款比例，10000 = 100%（密文）。
    pub multiplier_bps: String,
    /// fixed、smart、manual 或 sms；旧流水为空并按原有字段推断。
    pub strategy: String,
    /// 非聪明定投为空；指数行情本身不是用户隐私。
    pub index_code: String,
    pub moving_average_days: i32,
    pub quote_date: Option<Date>,
    pub index_close: String,
    pub moving_average: String,
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
    #[sea_orm(
        belongs_to = "super::bill::Entity",
        from = "Column::FeeBillId",
        to = "super::bill::Column::Id",
        on_delete = "SetNull"
    )]
    FeeBill,
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

impl Related<super::bill::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::FeeBill.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
