use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "debt_records")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub person_id: i64,
    pub account_id: i64,
    /// "lend" | "borrow" | "repayment_received" | "repayment_paid"
    pub kind: String,
    /// 金额（整数分密文）
    pub amount: String,
    pub note: String,
    pub happened_at: DateTime,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::debt_person::Entity",
        from = "Column::PersonId",
        to = "super::debt_person::Column::Id",
        on_delete = "Cascade"
    )]
    Person,
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::AccountId",
        to = "super::account::Column::Id",
        on_delete = "Cascade"
    )]
    Account,
}

impl Related<super::debt_person::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Person.def()
    }
}

impl Related<super::account::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Account.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
