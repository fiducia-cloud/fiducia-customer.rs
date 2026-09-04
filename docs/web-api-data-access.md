# Web/API data-access decision

Tracking: [DEN-3893](https://linear.app/denman/issue/DEN-3893/fiducia-cloud-document-webapi-data-ownership-and-transport-decision), [DEN-3960](https://linear.app/denman/issue/DEN-3960/document-4-web-server-to-api-server-data-access-patterns-across-10)

Portfolio authority: [web-to-API data-access ADR](https://github.com/ORESoftware/k8s-cluster/blob/main/docs/architecture/web-api-data-access.md)

## Scope

This record covers the traditional Fiducia customer web/BFF and API boundary. It does **not** govern
`fiducia-brain`, `fiducia-node`, Raft, routing, or coordination-plane storage; those services retain
their specialized cluster and protocol decisions.

## Current topology and selected paths

`fiducia-customer.rs` currently combines presentation and a narrow customer API/BFF in one process.
Its local customer profile, preferences, sessions, notifications, audit projection, billing webhook
ledger, and idempotency rows use one API-owned customer database connection. These operations are
not presented as P1 between separately trusted web and API tiers: the process is already the
authenticated API transaction boundary and the connection can perform narrowly granted customer
DML.

For a future physical split, **P2, stateless HTTP**, is the default. Customer writes and business
invariants move behind the customer API deployment. API-key create/list/rotate/revoke and customer
credential verification already use P2 to `fiducia-auth`, the sole credential authority. Requests
carry the verified customer identity and stable idempotency key where required; a dependency failure
never falls back to the customer database as a second credential authority.

**P1, direct read-only database access**, is allowed only for a measured shared-domain projection
after the `fiducia-orm-core` boundary described in [`database-boundary.md`](database-boundary.md) is
complete. It requires a separate `FIDUCIA_SHARED_READ_DATABASE_URL`, exact SELECT-only login,
verified `default_transaction_read_only=on`, opaque named policy-aware queries, bounded results, and
tenant scope derived from verified identity. The current customer writer is not a P1 credential and
must never be reused for shared coordination-domain reads.

The browser `/app/ws` and `/app/events` transports are heartbeat/refresh channels, not P3 between
the web and API clusters. They carry no customer rows or credentials; the browser reloads durable
state through authenticated routes. A future P3 API connection requires versioned bounded frames,
handshake and expiry authorization, operation IDs, heartbeat, idle/maximum lifetime, cursor resume,
reconnect jitter, slow-consumer behavior, and deployment drain.

This customer process has no NATS credential and no P4 operation. Future customer imports,
notification delivery, billing reconciliation, or other durable work may use P4 only behind the API
boundary with a versioned envelope, transactional outbox/inbox, stable operation ID, deadline,
bounded delivery/concurrency, dead-letter recovery, trace context, and durable result lookup. P4 is
asynchronous work, not a hidden synchronous replacement for P2.

## Operation map

| Operation | Path | Owner and contract |
| --- | --- | --- |
| Customer profile/preferences/session/notification state today | Combined API-owned database | Authenticated tenant-scoped handler and exact customer-table DML; not a P1 claim |
| Verify customer session and API-key lifecycle | P2 | `fiducia-auth` owns credential policy, writes, idempotency result, and sanitized metadata |
| Future separately deployed customer web command/query | P2 default | Typed API, audience-bound identity, end-to-end deadline, bounded body/result |
| Future optimized shared-domain customer view | P1 only after evidence | Separate verified SELECT-only context and named tenant-safe query |
| Browser WebSocket/SSE refresh | Browser channel, not P3 | Non-sensitive wakeup; authoritative state reloads through authenticated routes |
| Future sustained API subscription | P3 only after protocol review | Bounded, resumable, reauthorized, drainable connection |
| Future durable customer/background command | P4 only through API | Outbox/inbox, replay-safe worker, dead-letter and queryable result |

## Security, retry, backpressure, and observability

- The combined customer database login is isolated from the admin database and specialized Fiducia
  coordination cluster. It gets only customer-table DML and no broad DDL, ownership, role-switch, or
  coordination-domain authority.
- P2 calls remain inside the 30-second ingress timeout. The shared auth verifier also has its own
  10-second upstream timeout. Safe reads may retry transient failures with jitter; writes retry only
  with a stable idempotency key whose customer, operation, and request fingerprint match the stored
  result.
- Future P1 bounds pool, acquire and statement time, rows, and output. A failed privilege/policy
  assertion fails closed and cannot substitute the current customer writer.
- W3C trace context and an opaque request/operation ID cross P2 and link database spans. Metrics use
  fixed path/operation/outcome labels, never customer, organization, session, key, billing, or
  payload identifiers. Logs and stream frames exclude credentials and customer rows.
- Graceful shutdown stops HTTP admission and drains bounded in-flight routes plus browser streams.
  P3 or P4 cannot be enabled without their additional connection/subscription drain contracts.

## Schema and migration authority

`fiducia-interfaces/sql/customer.sql` is the declarative customer-plane schema authority;
`fiducia-interfaces` supplies generated database and wire contracts. Shared Fiducia definitions are
imported through the canonical `fiducia-orm-core`/`ORESoftware/k8s-libs-and-shared-defs` boundary,
not copied into handlers. [`scripts/dpm-schema.sh`](../scripts/dpm-schema.sh) diffs and verifies the
customer schema, and a separate human-approved migration identity applies it. Long-lived customer
web/API replicas do not receive DDL authority, synthesize schema from SeaORM models, or migrate at
startup.
