//! SeaORM entity for the canonical customer-plane `org_members` table.
//!
//! Tenant membership is application-owned authorization state. Shared Auth proves
//! identity/session assurance; it must not mint or substitute these rows.
//!
//! This entity lands one stack layer before the strict Shared Auth consumer cutover,
//! so the binary does not query it yet. Keep the exception local to this module and
//! remove it when DEN-1379 wires `authenticate_shared` to the membership lookup.
#![allow(dead_code)]

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "org_members")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub org_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: Uuid,
    pub role: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
