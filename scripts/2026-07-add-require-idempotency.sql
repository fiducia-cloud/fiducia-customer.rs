-- Migration: persist per-key `require_idempotency` on the customer api_keys table.
--
-- Context: the backend admin API already accepts and echoes `require_idempotency`
-- (see create_customer_api_key in src/main.rs), but it is NOT persisted — the
-- INSERT binds only key_id/org_id/name/secret_hash/scopes/env. Until this column
-- exists and is bound, the flag is cosmetic and the edge/LB cannot enforce it.
--
-- The default is TRUE to match the admin UI's `default_require_idempotency: true`.
-- Existing rows adopt the safe-by-default posture (require a key on mutations).
--
-- REVIEW BEFORE RUNNING: this is a customer-DB schema change. Run it together with
-- the backend code changes that bind/read the column (see
-- docs/require-idempotency-wiring.md) — landing the code without the column, or
-- vice versa, will break api_keys writes.

ALTER TABLE api_keys
    ADD COLUMN IF NOT EXISTS require_idempotency boolean NOT NULL DEFAULT true;
