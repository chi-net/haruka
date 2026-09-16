use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "preferences")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub default_currency: String,
    /// 用于识别本人转账和本人信用卡还款短信的姓名密文。
    pub owner_name: String,
    /// 自定义短信 API Token 的 SHA-256 校验值；空值使用启动配置或兼容默认值。
    pub sms_api_token_hash: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
