//! SeaORM entity for the customer `api_keys` table. Hand-written (the schema is
//! declarative — see `fiducia-interfaces/sql/customer.sql`), so no migration
//! tooling is involved. Columns mirror the canonical DDL and the shared
//! [`fiducia_interfaces_db::customer::ApiKeysRow`] contract; the store seam maps
//! this `Model` back to `ApiKeysRow` so callers stay engine-agnostic.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "api_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub key_id: String,
    pub org_id: Uuid,
    pub project_id: Option<Uuid>,
    pub created_by_user_id: Option<Uuid>,
    pub name: String,
    pub secret_hash: String,
    pub scopes: Json,
    pub env: String,
    pub mtls_required: bool,
    pub revoked: bool,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    pub version: i64,
    pub last_used_at: Option<DateTimeWithTimeZone>,
    pub expires_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// Map the ORM model to the shared `ApiKeysRow` contract type returned by the
    /// store seam, so handlers/broadcast/tests are decoupled from the engine.
    pub fn into_row(self) -> fiducia_interfaces_db::customer::ApiKeysRow {
        fiducia_interfaces_db::customer::ApiKeysRow {
            id: self.id,
            key_id: self.key_id,
            org_id: self.org_id,
            project_id: self.project_id,
            created_by_user_id: self.created_by_user_id,
            name: self.name,
            secret_hash: self.secret_hash,
            scopes: self.scopes,
            env: self.env,
            mtls_required: self.mtls_required,
            revoked: self.revoked,
            created_at: self.created_at.into(),
            updated_at: self.updated_at.into(),
            version: self.version,
            last_used_at: self.last_used_at.map(Into::into),
            expires_at: self.expires_at.map(Into::into),
        }
    }
}
