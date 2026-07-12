# Wiring `require_idempotency` end to end

The idempotency-key **enforcement mechanism** is already implemented and tested in
`fiducia-interfaces`, `fiducia-auth`, and `fiducia-load-balance` (see below). It is
a safe no-op today because the per-key flag is never persisted, so introspection
always reports it absent → the LB treats every key as "not required".

This note is the remaining last mile: persist the flag and propagate it to the
edge. Land it as one reviewed change together with the migration
(`scripts/2026-07-add-require-idempotency.sql`).

## Already done (mechanism — no action needed)

- **fiducia-interfaces** — `Introspection.require_idempotency: Option<bool>` added
  to `schema/common.schema.json` and regenerated (drift check passes).
- **fiducia-auth** — `require_idempotency` threaded through `StoredKey` (KV record,
  `#[serde(default)]` so old records read `false`) → `ApiKeyRecord` → the
  `Introspection` returned by `KeyStore::introspect`.
- **fiducia-load-balance** — `VerifiedIdentity.require_idempotency`, mapped from
  introspection, enforced in `proxy::route`: a mutating call (POST/PUT/PATCH/DELETE,
  excluding the `/v1/idempotency/*` primitives) from a key that requires it and
  carries no `Idempotency-Key` gets `400 idempotency_key_required`. Covered by
  `require_idempotency_rejects_keyless_mutation_before_routing`,
  `require_idempotency_allows_mutation_carrying_a_key`,
  `require_idempotency_does_not_gate_reads`.

## Remaining — backend persistence (this repo)

1. **Migration** — run `scripts/2026-07-add-require-idempotency.sql`
   (`ALTER TABLE api_keys ADD COLUMN require_idempotency boolean NOT NULL DEFAULT true`).
2. **`ApiKeysRow`** — add `require_idempotency: bool` (the INSERT uses `returning *`,
   so the row must carry it to be read back).
3. **INSERT** (`create_customer_api_key`, ~src/main.rs:555) — add the column and
   bind `payload.require_idempotency.unwrap_or(true)`:
   ```
   insert into api_keys (key_id, org_id, name, secret_hash, scopes, env, require_idempotency)
   values ($1, $2, $3, $4, $5, $6, $7) returning *
   ```
4. **Response** — build `api_key["require_idempotency"]` from the row (main.rs:375,402)
   instead of `payload...unwrap_or(true)`, so the persisted value is the source of truth.
5. **List / rotate / update** queries — already `select *` / `returning *`; just make
   sure any hand-built JSON includes the row's value.

## Remaining — propagation to the edge (VERIFY the path)

`fiducia-auth` reads key records from the **node KV store** (`FIDUCIA_KV_URL`), not
from this backend's Postgres directly. Confirm how an `api_keys` row reaches that KV
so the new column travels with it. Candidates observed in the code:

- `sync_write_api_keys` + the `fiducia:sync` broadcast (src/main.rs:457,615) — verify
  whether these frames (a) only fan out to dashboard WS/SSE subscribers, or (b) also
  land the record in node KV that auth reads. If (a), the KV write happens elsewhere.
- `fiducia-auth`'s own `KeyStore::create` writes `StoredKey` to node KV directly — if
  admin-created keys go through a *different* path than auth-created keys, both writers
  must include `require_idempotency` (auth's `StoredKey` already does).

Whichever writer populates the auth-readable KV record must serialize
`require_idempotency`. Because `StoredKey` uses `#[serde(default)]`, missing it is safe
(reads as `false`) — enforcement simply stays off until the value flows.

## Rollout

Default is `true`, so once persisted+propagated, existing keys begin requiring an
`Idempotency-Key` on mutations. Clients already send stable keys across retries
(fiducia-clients PR #8), so this should be safe — but roll out behind awareness, and
consider defaulting new-but-not-yet-migrated environments to `false` first if a
softer ramp is wanted.
