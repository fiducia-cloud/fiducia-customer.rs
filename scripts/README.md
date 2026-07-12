# scripts

SQL against the **customer Postgres plane** that backs the `api_keys` vertical
(used when `DATABASE_URL` is set). Review before running — these touch customer
data/schema.

- **`seed-customer.sql`** — idempotent seed: one org plus a few `api_keys` rows so
  `GET /api/customer/api-keys` returns real DB rows for local/portal development.
- **`2026-07-add-require-idempotency.sql`** — migration adding the
  `require_idempotency` column (default `true`) to `api_keys`. Must land together
  with the backend code that binds/reads it (see
  `docs/require-idempotency-wiring.md`).
