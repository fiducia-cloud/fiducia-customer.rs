use sea_orm::entity::prelude::*;
use uuid::Uuid;

/// Canonical customer-to-organization authorization edge.
///
/// Shared Auth proves identity and session assurance; this table remains the
/// only authority that grants a verified subject access to a Fiducia tenant.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "org_members")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub org_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: Uuid,
    pub role: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
