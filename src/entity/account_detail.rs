use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "account_details")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: i64,
    pub card_number: String,
    pub account_username: String,
    /// 授信额（整数分密文），非信用类账户加密保存 0
    pub credit_limit: String,
    /// 账单日 1..=31，非信用类账户为 0
    pub billing_day: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::AccountId",
        to = "super::account::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Account,
}

impl Related<super::account::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Account.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
