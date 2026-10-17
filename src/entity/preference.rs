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
    /// 公开明文网站页脚，不受账务数据加密保护。
    #[sea_orm(column_type = "Text", default_value = "")]
    pub footer_text: String,
    /// 公开明文 ICP 备案号，仅链接到工信部备案网站。
    #[sea_orm(column_type = "Text", default_value = "")]
    pub icp_record: String,
    /// 公开明文公安备案号，仅链接到固定公安备案网站。
    #[sea_orm(column_type = "Text", default_value = "")]
    pub public_security_record: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
