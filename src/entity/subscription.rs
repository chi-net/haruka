use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "subscriptions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    /// 每次订阅支出金额（整数分密文）
    pub amount: String,
    pub currency: String,
    pub category: String,
    /// "day" | "week" | "month" | "quarter" | "year"
    pub period: String,
    pub expires_at: DateTime,
    /// 绑定后由已解锁客户端在到期时自动生成支出；账户必须与订阅同币种。
    pub auto_debit_account_id: Option<i64>,
    /// 到期前多少个自然日开始检查自动扣款余额，范围 0..=30。
    pub balance_check_days: i32,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
