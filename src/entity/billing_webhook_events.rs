//! SeaORM projection of the `billing_webhook_events` ledger (see
//! `fiducia-interfaces/sql/customer.sql`).
//!
//! This is the exactly-once + signature-verification control surface for inbound
//! Stripe/PayPal webhooks. The unique `(provider, provider_event_id)` index makes
//! an INSERT the idempotency primitive: a provider's at-least-once redelivery
//! either inserts a fresh row (process it) or conflicts (already handled).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "billing_webhook_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub provider: String,
    pub provider_event_id: String,
    pub event_type: String,
    pub signature_verified: bool,
    pub payload_sha256: String,
    pub received_at: DateTimeWithTimeZone,
    pub processed_at: Option<DateTimeWithTimeZone>,
    pub process_error: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
