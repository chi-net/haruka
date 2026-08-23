use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "exchange_rates")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub requested_date: Date,
    pub rate_date: Date,
    pub base_currency: String,
    pub quote_currency: String,
    /// 十进制定点字符串；汇率属于公开市场数据，不使用 DEK 加密。
    pub rate: String,
    pub fetched_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
