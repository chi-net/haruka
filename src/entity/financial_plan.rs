use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "financial_plans")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 加密后的理财规划配置及关联定投记录。
    pub payload: String,
    pub revision: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
