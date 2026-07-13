-- Migration: persist per-key `require_idempotency` on the customer api_keys table.
--
-- Compatibility path for databases created before the canonical
-- fiducia-interfaces customer schema gained this column. Current SeaORM entities,
-- auth issuance, and edge introspection all bind/read the policy end to end.
--
-- The default is TRUE to match the customer MASH server's issuance default.
-- Existing rows adopt the safe-by-default posture (require a key on mutations).
--
-- REVIEW BEFORE RUNNING: this is a customer-DB schema change. Fresh databases get
-- the same idempotent ALTER from fiducia-interfaces/sql/customer.sql.

ALTER TABLE api_keys
    ADD COLUMN IF NOT EXISTS require_idempotency boolean NOT NULL DEFAULT true;
