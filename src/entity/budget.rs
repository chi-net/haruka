use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "budgets")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    /// 每日预算金额（分）的密文；空值表示未启用。
    pub daily_amount: String,
    /// 每周预算金额（分）的密文；空值表示未启用。
    pub weekly_amount: String,
    /// 每月预算金额（分）的密文；空值表示未启用。
    pub monthly_amount: String,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
