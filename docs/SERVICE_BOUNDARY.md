# Customer web/API service boundary

This decision applies to Fiducia's traditional customer web/API services such
as `fiducia-customer.rs`, auth, routing, payment, and operations control-plane
services. It intentionally excludes `fiducia-brain.rs` and `fiducia-node.rs`,
whose distributed-node protocols have different correctness requirements.

| Connection | Allowed use | Boundary |
| --- | --- | --- |
| Direct database read | public/cache-safe customer catalog projections only | never tenant state, credentials, policy, leases, or payment data |
| Stateless HTTP/JSON | normal customer/UI to control-plane API operations | API verifies tenant/actor authorization and enforces idempotency |
| Stateful TCP | mTLS subscriptions after an HTTP-authorized setup | use for live updates, not command acceptance or durable state |
| NATS/MQ | transactional-outbox notifications, audits, and async workflows | never lock/lease grant, auth, billing result, or request authority |

Fiducia clients may use its lock/lease API as a liveness complement, but a
lease never replaces a durable database uniqueness/transaction guard. Do not
forward bearer credentials across redirects or treat a broker response as proof
that a customer mutation committed.
