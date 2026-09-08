# Fiducia customer formal models

## Billing webhook ingestion

`billing_webhook_ingest.qnt` is a bounded executable model of the
signature-verification and idempotent-ingest boundary in `src/billing.rs`.

### Production refinement map

| Model concept | Production symbol or boundary |
| --- | --- |
| `signature_valid` | successful `billing::verify`, implemented by `fiducia-payments.rs` |
| `record_fresh` | `billing::record` successful SeaORM insert |
| durable identity | unique `(provider, provider_event_id)` row |
| stored digest | `billing_webhook_events.payload_sha256` |
| stored event type | `billing_webhook_events.event_type` |
| `classify_exact` | `billing::is_exact_redelivery` returns true |
| `classify_conflict` | the existing row differs in verified flag, provider, event ID, event type, or digest |
| `effects` | rows or downstream effects created by the current delivery |
| `storage_failure` | unavailable connection or SeaORM failure |

The production code hashes `VerifiedEvent.payload`, which is the exact byte
sequence retained by the successful verifier. It does not hash a separately
re-read or reparsed request body.

### Safety properties within the finite model

- an unverified delivery cannot be recorded;
- the current delivery creates at most one durable effect;
- a fresh authenticated delivery records exactly its digest and event type;
- a deduped delivery must exactly match the stored authenticated identity,
  event type, and digest;
- reuse of an existing provider event identity with a different digest or event
  type reaches an explicit conflict and creates no effect;
- storage failure creates no effect;
- retries after a terminal classification are observational no-ops.

### Bounds and assumptions

The model uses two representative payload digests and two representative event
types, one optional existing row, one current delivery, and at most one current
effect. The small domain is sufficient for equality/inequality and
exactly-once classification; it is not a claim about SHA-256 collision
resistance.

The verifier's cryptographic correctness, provider signing-key custody,
certificate PKI, database durability/isolation, network delivery, scheduler
fairness, and downstream payment processing remain explicit assumptions.
Existing PostgreSQL uniqueness must remain enforced. Concrete downstream
effects must consume only `Recorded`; `Deduped` is a no-op and
`IdentityConflict` is non-success.

`billing_webhook_ingest_test.qnt` supplies deterministic witnesses. The Rust
unit tests in `src/billing.rs` execute the production digest and
exact-redelivery classifier. CI typechecks and simulates the model on pull
requests and runs bounded Apalache verification after merge, on the weekly
schedule, and on manual invocation.
