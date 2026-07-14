//! SeaORM entities for the customer plane. Hand-written against the declarative
//! schema in `fiducia-interfaces/sql/customer.sql`; operators converge that
//! schema with the DPM workflow documented in this repository.

pub mod audit_log;
pub mod customer_preferences;
pub mod customer_sessions;
pub mod users;
