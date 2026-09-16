use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "debt_people")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    pub note: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::debt_record::Entity")]
    DebtRecord,
}

impl Related<super::debt_record::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DebtRecord.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
