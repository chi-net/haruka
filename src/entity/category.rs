use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "categories")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// "income" | "expense"
    pub kind: String,
    pub name: String,
    /// 是否计入恩格尔系数的食品支出
    pub is_food: bool,
    /// 每周期支出笔数限额的密文；空字符串表示停用
    #[sea_orm(default_value = "")]
    pub count_limit: String,
    /// "day" | "week" | "month"，自然周期按访问者时区解释
    #[sea_orm(default_value = "month")]
    pub count_limit_period: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
