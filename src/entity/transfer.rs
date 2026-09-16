use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "transfers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub from_account_id: i64,
    pub to_account_id: i64,
    /// 金额（整数分密文）
    pub amount: String,
    /// 转入账户实际收到的金额（整数分密文）；跨币种转账与 amount 不同。
    pub to_amount: String,
    pub note: String,
    pub happened_at: DateTime,
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
        from = "Column::ToAccountId",
        to = "super::account::Column::Id",
        on_delete = "Cascade"
    )]
    ToAccount,
}

impl ActiveModelBehavior for ActiveModel {}
