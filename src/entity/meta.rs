use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "meta")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub salt: Vec<u8>,
    pub dek_nonce: Vec<u8>,
    pub wrapped_dek: Vec<u8>,
    /// Completed password-failure rounds; three permanently disable password verification.
    #[sea_orm(default_value = 0)]
    pub failed_password_rounds: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
