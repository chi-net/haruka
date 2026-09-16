use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "market_index_quotes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub index_code: String,
    pub trade_date: Date,
    /// 公开指数的收盘点位，使用 Decimal 字符串避免浮点误差。
    pub close: String,
    pub source: String,
    pub fetched_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
