# Customer-plane and shared-domain database boundary

Tracking: [DEN-2789](https://linear.app/denman/issue/DEN-2789/fiducia-cloud-shared-seaorm-boundary-fiducia-orm-in-fiducia-lib-fiducia)

Canonical ORM owner: [`fiducia-cloud/fiducia-orm-core`](https://github.com/fiducia-cloud/fiducia-orm-core), currently under review in [PR #1](https://github.com/fiducia-cloud/fiducia-orm-core/pull/1).

`fiducia-customer.rs` is currently a mixed customer-facing BFF: it renders the customer experience and also exposes narrowly scoped authenticated customer API endpoints. That makes two different data boundaries visible in one deployable. They must remain explicit until the write endpoints move behind a dedicated API service.

## 1. Customer-owned writable state

The existing `DATABASE_URL` and `connect_customer_db()` path serve the customer plane owned by this deployable. Current examples include the local customer/user projection, preferences, browser-observed sessions, notification state, and the customer audit projection used by its authenticated endpoints.

This write capability is constrained as follows:

- it is not a general Fiducia coordination or credential-authority connection;
- writes are reachable only through authenticated, tenant-scoped API/BFF handlers, never directly from rendering helpers;
- API-key lifecycle and credential authority remain in `fiducia-auth` and are called over HTTP;
- the customer runtime principal receives only the DML required by the customer-owned tables and no broad DDL rights;
- customer schema changes run as a discrete reviewed release step, not as automatic ORM synchronization at process boot;
- this customer writer credential is never shared with a traditional read-only web server or with the specialized Fiducia coordination cluster.

The long-term topology should split these mutations into a dedicated customer API service. Until then, this repository must be classified as a combined BFF/API deployable rather than cited as an example of a read-only traditional web tier.

## 2. Shared Fiducia domain reads

Coordination, lease, lock, consensus, node, brain, and other shared Fiducia domain data remain owned by their API/service boundary. This customer deployable may read an approved projection only when all of the following hold:

- a separate `FIDUCIA_SHARED_READ_DATABASE_URL` is used; never reuse `DATABASE_URL`;
- the principal has schema `USAGE` and an explicit `SELECT` allowlist only;
- the connection is created through the default `read-only` surface of `fiducia-orm-core`;
- `default_transaction_read_only=on` is verified by the crate before the opaque read context enters application state;
- handlers call named policy-aware read functions, not a raw SeaORM connection, entity manager, or query builder;
- every query is tenant-scoped, bounded, and redacted;
- shared-domain mutations always go through the owning API/service over authenticated HTTP or another explicit versioned contract.

If a read-only shared connection cannot be verified, the affected shared-domain view must fail closed or degrade to an unavailable state. The process must not fall back to the customer writer credential.

## Canonical ORM rollout

The root `.zpkg.toml` imports `fiducia-cloud/fiducia-orm-core`. After `fiducia-orm-core` PR #1 is completed and private Cargo authentication is available in every build environment, the intended dependency is:

```toml
fiducia-orm-core = {
  git = "https://github.com/fiducia-cloud/fiducia-orm-core.git",
  rev = "3fc2c5e5ba17cefea5fee00a1b77578929e48d1f",
  default-features = false,
  features = ["read-only"]
}
```

The future shared-read seam should expose only an opaque context, for example:

```rust
use fiducia_orm_core::{ReadContext, connect_read_only};

let shared_reads: ReadContext = connect_read_only(
    &std::env::var("FIDUCIA_SHARED_READ_DATABASE_URL")?,
).await?;
```

The revision above is the current head of the canonical scaffold PR, not yet a production-ready consumer pin. Before enabling it, that PR must pin the Fiducia entity slice from `ORESoftware/k8s-libs-and-shared-defs`, keep raw ORM/session types private, implement role-aware connection and read-only verification, add working named queries, compile every write type/function only under `read-write`, and provide compile-fail consumer fixtures plus live PostgreSQL/CockroachDB denial evidence. Replace the scaffold revision with the merge commit after those gates pass.

The earlier `fiducia-lib` ORM branch is an implementation donor only and must not become a second authoritative package. The Cargo git dependency is documented rather than committed before the canonical private dependency can be fetched and `Cargo.lock` regenerated reproducibly. The zed package dependency records the correct cross-repository relationship now.

## Schema and migration ownership

- Shared definitions are imported from `ORESoftware/k8s-libs-and-shared-defs` through the canonical ORM crate, namespaced by organization and project.
- Shared Fiducia schema changes use `declarative-migrations`/`dpm`, are verified against PostgreSQL and CockroachDB as applicable, and run under a separate migrator identity.
- Destructive changes follow expand → backfill → contract across compatible releases.
- `fiducia-node.rs`, `fiducia-brain.rs`, and similar coordination services remain on the specialized Fiducia Kubernetes cluster.
- Traditional customer/web/API deployables run through `ORESoftware/k8s-cluster` and do not share broad database credentials with the specialized cluster.

## Non-negotiable negative rules

This repository must never:

- use its customer `DATABASE_URL` for shared-domain reads as a convenience fallback;
- enable the ORM crate's `read-write` feature or call a shared coordination-domain write function;
- acquire the shared API runtime or migrator credential;
- add automatic schema synchronization at startup;
- let rendering modules issue ad hoc SQL or bypass the authenticated customer/API boundary;
- allow the two Kubernetes clusters to mutate the same schema with shared credentials.
