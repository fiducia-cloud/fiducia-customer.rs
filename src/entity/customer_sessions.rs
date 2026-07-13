//! SeaORM entity for `customer_sessions` — trusted sessions shown on the
//! Security page (list + revoke). Schema: `fiducia-interfaces/sql/customer.sql`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "customer_sessions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub device: String,
    pub location: Option<String>,
    pub last_seen: DateTimeWithTimeZone,
    pub status: String,
    pub updated_at: DateTimeWithTimeZone,
    pub version: i64,
    pub sync_sequence: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// Map to the shared `fiducia-interfaces` contract row.
    pub fn into_row(self) -> fiducia_interfaces_db::customer::CustomerSessionsRow {
        fiducia_interfaces_db::customer::CustomerSessionsRow {
            id: self.id,
            user_id: self.user_id,
            device: self.device,
            location: self.location,
            last_seen: self.last_seen.into(),
            status: self.status,
            updated_at: self.updated_at.into(),
            version: self.version,
            sync_sequence: self.sync_sequence,
        }
    }
}
