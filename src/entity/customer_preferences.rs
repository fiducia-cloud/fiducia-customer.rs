//! SeaORM entity for `customer_preferences` (one row per user). The BEFORE
//! UPDATE trigger bumps `version` + `updated_at`. Schema:
//! `fiducia-interfaces/sql/customer.sql`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "customer_preferences")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: Uuid,
    pub density: String,
    pub timezone: String,
    pub region: String,
    pub notify_key_rotation: bool,
    pub notify_lock_contention: bool,
    pub notify_mfa: bool,
    pub updated_at: DateTimeWithTimeZone,
    pub version: i64,
    pub sync_sequence: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// Map to the shared `fiducia-interfaces` customer contract row.
    pub fn into_row(self) -> fiducia_interfaces_db::customer::CustomerPreferencesRow {
        fiducia_interfaces_db::customer::CustomerPreferencesRow {
            user_id: self.user_id,
            density: self.density,
            timezone: self.timezone,
            region: self.region,
            notify_key_rotation: self.notify_key_rotation,
            notify_lock_contention: self.notify_lock_contention,
            notify_mfa: self.notify_mfa,
            updated_at: self.updated_at.into(),
            version: self.version,
            sync_sequence: self.sync_sequence,
        }
    }
}
