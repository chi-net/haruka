use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "installment_plans")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub bill_id: Option<i64>,
    pub account_id: i64,
    /// 账单分期本金快照；旧消费分期为空时取原账单金额。
    #[sea_orm(default_value = "")]
    pub principal: String,
    pub repayment_transfer_id: Option<i64>,
    pub converted_at: Option<DateTime>,
    #[sea_orm(default_value = "")]
    pub refund_reviewed_amount: String,
    pub term_months: i32,
    /// "equal_payment" | "equal_principal" | "flat"
    pub method: String,
    /// 年利率基点（1 bp = 0.01%）密文
    pub annual_rate_bps: String,
    /// 整个分期计划的总手续费（整数分）密文
    pub fee: String,
    pub first_due_date: Date,
    pub deleted_at: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::bill::Entity",
        from = "Column::BillId",
        to = "super::bill::Column::Id",
        on_delete = "Cascade"
    )]
    Bill,
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::AccountId",
        to = "super::account::Column::Id",
        on_delete = "Cascade"
    )]
    Account,
    #[sea_orm(has_many = "super::installment_item::Entity")]
    Items,
}

impl Related<super::bill::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Bill.def()
    }
}

impl Related<super::account::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Account.def()
    }
}

impl Related<super::installment_item::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Items.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
