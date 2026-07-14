//! SeaORM projection of the customer-visible, append-only `audit_log` table.
//!
//! The portal intentionally selects only the fields safe for a customer-facing
//! activity feed. Network addresses, user agents, and arbitrary audit metadata
//! stay out of this model so they cannot accidentally become part of the BFF
//! response contract.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "audit_log")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub org_id: Option<Uuid>,
    pub actor_user_id: Option<Uuid>,
    pub actor: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub request_id: Option<String>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
