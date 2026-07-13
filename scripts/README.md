# scripts

SQL against the **customer Postgres plane** that backs the `api_keys` vertical
(used when `DATABASE_URL` is set). Review before running — these touch customer
data/schema.

- **`seed-customer.sql`** — idempotent seed: one org plus a few `api_keys` rows so
  `GET /api/customer/api-keys` returns real DB rows for local/portal development.
- **`2026-07-add-require-idempotency.sql`** — compatibility migration for older
  customer databases. Fresh databases receive the same column from the canonical
  `fiducia-interfaces/sql/customer.sql`; SeaORM binds and reads it.
- **`with-flags2env.sh`** — bridges CLI flags to the environment variables this
    backend reads (`PORT`, marketing `STATIC_DIR`, `FIDUCIA_*`, and public
    `SUPABASE_*` settings). Database credentials and debug auth remain
    environment-only. It runs the pinned `flags2env` parser
  (`vendor/flags-2-env`) against the `.cli-flags.toml` schema, exports the
  resulting env map, then execs the given command
  (e.g. `scripts/with-flags2env.sh --port 8080 --static-dir ../fiducia-ui.web/dist -- cargo run`).
  Build the parser first with `make -B -C vendor/flags-2-env all`.
