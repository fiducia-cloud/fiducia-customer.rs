# Customer-plane and shared-domain database boundary

Tracking: [DEN-2789](https://linear.app/denman/issue/DEN-2789/fiducia-cloud-shared-seaorm-boundary-fiducia-orm-in-fiducia-lib-fiducia)

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
- the connection is created through `fiducia-orm` with `DbRole::ReadOnly`;
- `default_transaction_read_only=on` is verified with `assert_read_only` before the connection enters application state;
- handlers call named functions under `fiducia_orm::queries::read`, not an unrestricted ORM session or query builder;
- every query is tenant-scoped, bounded, and redacted;
- shared-domain mutations always go through the owning API/service over authenticated HTTP or another explicit versioned contract.

If a read-only shared connection cannot be verified, the affected shared-domain view must fail closed or degrade to an unavailable state. The process must not fall back to the customer writer credential.

## Shared library rollout

The root `.zpkg.toml` imports `fiducia-cloud/fiducia-lib`. `fiducia-lib` PR #1 adds `fiducia-orm`, including `DbRole::{ReadWrite, ReadOnly}`, `assert_read_only`, `ORG_SCHEMA = "fiducia"`, schema-qualified helpers, and named read/write query modules.

After that PR merges and private Cargo authentication is available in every build environment, the intended Cargo dependency is:

```toml
fiducia-orm = {
  package = "fiducia-orm",
  git = "https://github.com/fiducia-cloud/fiducia-lib.git",
  rev = "03cf218db1dcfc96f08d49dabcf19d447869644f"
}
```

The revision above is the reviewed head of `fiducia-lib` PR #1. Replace it with the merge commit before enabling the dependency. The future shared-read seam is:

```rust
use fiducia_orm::{DbRole, assert_read_only, connect};

let shared_reads = connect(
    &std::env::var("FIDUCIA_SHARED_READ_DATABASE_URL")?,
    DbRole::ReadOnly,
)
.await?;
assert_read_only(&shared_reads).await?;
```

The Cargo git dependency is documented rather than committed before the private dependency can be fetched and `Cargo.lock` regenerated reproducibly. The zed package dependency records the cross-repository relationship now.

## Schema and migration ownership

- Shared definitions are imported from `ORESoftware/k8s-libs-and-shared-defs` through the organization library, namespaced by organization and project.
- Shared Fiducia schema changes use `declarative-migrations`/`dpm`, are verified against PostgreSQL and CockroachDB as applicable, and run under a separate migrator identity.
- Destructive changes follow expand → backfill → contract across compatible releases.
- `fiducia-node.rs`, `fiducia-brain.rs`, and similar coordination services remain on the specialized Fiducia Kubernetes cluster.
- Traditional customer/web/API deployables run through `ORESoftware/k8s-cluster` and do not share broad database credentials with the specialized cluster.

## Non-negotiable negative rules

This repository must never:

- use its customer `DATABASE_URL` for shared-domain reads as a convenience fallback;
- import or call `fiducia_orm::queries::write` for coordination-domain data;
- acquire the shared API runtime or migrator credential;
- add automatic schema synchronization at startup;
- let rendering modules issue ad hoc SQL or bypass the authenticated customer/API boundary;
- allow the two Kubernetes clusters to mutate the same schema with shared credentials.
