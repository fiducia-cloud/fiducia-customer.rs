-- Seed data for the DB-backed api_keys vertical (customer plane).
--
-- Idempotent: safe to run repeatedly. Gives the portal an org plus a couple of
-- API keys to render so `GET /api/customer/api-keys` returns real DB rows.
--
--   PGPASSWORD=fiducia psql -h 127.0.0.1 -p 5433 -U postgres \
--     -d fiducia_customer -f scripts/seed-customer.sql

insert into orgs (slug, name)
values ('acme', 'Acme Corp')
on conflict (slug) do nothing;

-- secret_hash is a SHA-256 of the (never-stored) plaintext secret; these seed
-- rows use placeholder hashes since their plaintext was never issued.
insert into api_keys (key_id, org_id, name, secret_hash, scopes, env)
select 'fid_live_seed0001', o.id, 'Production checkout',
       'sha256:seed-placeholder-0001',
       '["locks:write", "kv:read", "requests:write"]'::jsonb, 'live'
from orgs o
where o.slug = 'acme'
on conflict (key_id) do nothing;

insert into api_keys (key_id, org_id, name, secret_hash, scopes, env)
select 'fid_live_seed0002', o.id, 'Billing worker',
       'sha256:seed-placeholder-0002',
       '["locks:write", "services:read"]'::jsonb, 'live'
from orgs o
where o.slug = 'acme'
on conflict (key_id) do nothing;

insert into api_keys (key_id, org_id, name, secret_hash, scopes, env)
select 'fid_test_seed0003', o.id, 'Staging replay',
       'sha256:seed-placeholder-0003',
       '["requests:write", "kv:write"]'::jsonb, 'test'
from orgs o
where o.slug = 'acme'
on conflict (key_id) do nothing;
