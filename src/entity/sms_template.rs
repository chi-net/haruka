use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "sms_templates")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 模板名、发件号码、银行标记和正则表达式均为密文。
    pub name: String,
    pub sender: String,
    pub bank_tag: String,
    pub pattern: String,
    pub action: String,
    pub account_id: Option<i64>,
    pub plan_id: Option<i64>,
    pub category_id: Option<i64>,
    pub enabled: bool,
    pub created_at: DateTimeUtc,
}

// 不设置级联外键：删除引用对象后，处理短信时必须显式报错。
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
