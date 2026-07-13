-- Seed data for the DB-backed api_keys vertical (customer plane).
--
-- Idempotent: safe to run repeatedly. Seeds only a development organization;
-- credentials must be issued through the authenticated API so their plaintext
-- is shown once and their real hash is durably stored.
--
--   PGPASSWORD=fiducia psql -h 127.0.0.1 -p 5433 -U postgres \
--     -d fiducia_customer -f scripts/seed-customer.sql

insert into orgs (slug, name)
values ('acme', 'Acme Corp')
on conflict (slug) do nothing;
