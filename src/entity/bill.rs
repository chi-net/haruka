use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "bills")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub account_id: i64,
    /// "income" | "expense"
    pub kind: String,
    /// 金额（整数分）的密文，base64 编码
    pub amount: String,
    pub category: String,
    /// 稳定分类身份，删除分类后仍保留；NULL 仅用于待迁移旧账单，0 表示无法关联
    pub category_id: Option<i64>,
    /// 创建或修改账单时记录的食品类标记
    pub is_food: bool,
    /// 应用内隐私账单标记；金额仍参与真实余额，分类与备注仅隐私模式可读。
    #[sea_orm(default_value = false)]
    pub is_private: bool,
    /// 退款流水关联的原支出；余额保留流水，统计抵扣原支出而不计收入。
    pub refund_of_bill_id: Option<i64>,
    pub note: String,
    pub happened_at: DateTime,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::AccountId",
        to = "super::account::Column::Id",
        on_delete = "Cascade"
    )]
    Account,
}

impl Related<super::account::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Account.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
