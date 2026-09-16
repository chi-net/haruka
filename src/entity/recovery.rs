use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "recovery")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub dek_nonce: Vec<u8>,
    pub wrapped_dek: Vec<u8>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
