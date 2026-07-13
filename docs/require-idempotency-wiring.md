# `require_idempotency` end-to-end wiring

The key policy is now carried by the complete issuance path:

1. The customer MASH server accepts `require_idempotency` (default `true`).
2. It calls `fiducia-auth POST /v1/keys` with the verified Supabase bearer token,
   customer organization, scopes, environment, and policy.
3. `fiducia-auth` stores the authoritative verifier and policy in Fiducia KV.
4. The customer server mirrors sanitized metadata into the isolated customer
   Postgres plane through SeaORM.
5. `fiducia-auth /v1/introspect` returns the policy to the edge/load balancer,
   which rejects keyless mutating requests when it is enabled.

The canonical relational column lives in
`fiducia-interfaces/sql/customer.sql`; generated Rust and TypeScript row types
include it. `scripts/2026-07-add-require-idempotency.sql` remains only as a
compatibility migration for databases created before that canonical column.

Creation and rotation are fail-closed across the two stores. If the authoritative
auth write succeeds but the relational mirror fails, the customer server issues a
compensating auth revocation. The raw credential is shown once and never persisted
in Postgres.
