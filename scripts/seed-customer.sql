-- Seed data for the DB-backed api_keys vertical (customer plane).
--
-- Idempotent: safe to run repeatedly. Seeds the E2E fixture org — whose id is
-- fixed to match the debug-only static-session authenticator (auth.rs, enabled by
-- FIDUCIA_E2E_STATIC_CUSTOMER_AUTH) so the authenticated create/list has a real
-- org to attach rows to — plus one fixture key so the DB-backed list is non-trivial
-- and carries an id/version (the specs use that to confirm the real DB path, not
-- the mock path). The key's hash is an obvious placeholder, NOT a usable
-- credential — real keys are still issued through the authenticated API (plaintext
-- shown once, real hash durably stored).
--
--   PGPASSWORD=fiducia psql -h 127.0.0.1 -p 5433 -U postgres \
--     -d fiducia_customer -f scripts/seed-customer.sql

insert into orgs (id, slug, name)
values ('00000000-0000-4000-8000-000000000001', 'e2e-fixture-org', 'E2E Fixture Org')
on conflict (id) do nothing;

insert into api_keys (key_id, org_id, name, secret_hash, scopes, env)
values (
  'fid_live_e2e_seed',
  '00000000-0000-4000-8000-000000000001',
  'E2E Seed Key',
  'sha256:placeholder-not-a-real-credential',
  '["requests:write"]'::jsonb,
  'live'
)
on conflict (key_id) do nothing;
