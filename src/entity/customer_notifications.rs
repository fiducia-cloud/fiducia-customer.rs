//! SeaORM entity for `customer_notifications` (in-product notification feed,
//! one row per delivered notification, owned by a user). The BEFORE trigger
//! `customer_notifications_bump` stamps `version`/`updated_at`/`sync_sequence`;
//! `read_at` is the only field the portal edits after delivery. Schema:
//! `fiducia-interfaces/sql/customer.sql`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "customer_notifications")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub org_id: Option<Uuid>,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub read_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    pub version: i64,
    pub sync_sequence: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
