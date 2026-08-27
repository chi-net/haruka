use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "accounts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    /// "payment" | "bank" | "stored_value" | "investment" | "other"
    pub kind: String,
    /// ISO 4217 货币代码；账户内的金额均使用此币种。
    pub currency: String,
    /// 强制余额相对于各类资金记录净额的偏移量（整数分密文）
    pub balance_offset: String,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::bill::Entity")]
    Bill,
    #[sea_orm(has_one = "super::account_detail::Entity")]
    AccountDetail,
    #[sea_orm(has_many = "super::balance_adjustment::Entity")]
    BalanceAdjustment,
}

impl Related<super::bill::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Bill.def()
    }
}

impl Related<super::balance_adjustment::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::BalanceAdjustment.def()
    }
}

impl Related<super::account_detail::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AccountDetail.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
