use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "installment_items")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub plan_id: i64,
    pub sequence: i32,
    pub due_date: Date,
    /// 以下金额均为计划账户原币的整数分密文。
    pub principal: String,
    pub interest: String,
    pub fee: String,
    pub total: String,
    pub paid_at: Option<DateTimeUtc>,
    /// 实际还款使用的非信用账户，以及自动生成的本金转账和费用账单。
    pub repayment_account_id: Option<i64>,
    pub principal_transfer_id: Option<i64>,
    pub charge_bill_id: Option<i64>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::installment_plan::Entity",
        from = "Column::PlanId",
        to = "super::installment_plan::Column::Id",
        on_delete = "Cascade"
    )]
    Plan,
}

impl Related<super::installment_plan::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Plan.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
