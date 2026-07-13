//! SeaORM entities for the customer plane. Hand-written against the declarative
//! schema in `fiducia-interfaces/sql/customer.sql` (no migration tooling).

pub mod api_keys;
pub mod customer_preferences;
pub mod customer_sessions;
pub mod sync_idempotency_keys;
pub mod users;
