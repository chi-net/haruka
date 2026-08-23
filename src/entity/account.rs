use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "accounts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::bill::Entity")]
    Bill,
}

impl Related<super::bill::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Bill.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
