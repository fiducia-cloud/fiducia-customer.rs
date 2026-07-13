<!-- BEGIN k8s-cluster-submodule-notice -->
> [!NOTE]
> **Canonical source.** This repository is the source of truth for its code. It
> is also vendored as a **secondary** git submodule of
> [ORESoftware/k8s-cluster](https://github.com/ORESoftware/k8s-cluster) at
> `remote/deployments/fiducia-backend.rs` — make changes here, not in that submodule checkout.
>
> On disk: source clone `~/codes/fiducia.cloud/fiducia-backend.rs` · submodule checkout `~/codes/ores/k8s-cluster/remote/deployments/fiducia-backend.rs`.
<!-- END k8s-cluster-submodule-notice -->

# fiducia-backend

Rust + [axum](https://github.com/tokio-rs/axum) backend for **fiducia.cloud** —
"consensus & coordination as a service".

This is the **website tier only**: it serves the marketing site, the customer
portal shell, and a couple of health/info endpoints. It does **not** implement
coordination. The actual Raft-replicated coordination engine and its control
plane live in sibling repos:

- [`fiducia-node.rs`](https://github.com/fiducia-cloud/fiducia-node.rs) — data plane: sharded multi-Raft coordination (locks, rate limiting, cron, config KV + watches, leader election, service discovery).
- [`fiducia-brain.rs`](https://github.com/fiducia-cloud/fiducia-brain.rs) — control plane: shard placement, scaling, and node-failure handling.

It serves two things:

| Path        | Served                                                              |
|-------------|---------------------------------------------------------------------|
| `/healthz`, `/api/health` | health probe                                          |
| `/api/info` | service / version JSON                                              |
| `/app`, `/app/*` | customer portal rendered by axum + Maud and refreshed by HTMX |
| `/app/ws` | customer portal WebSocket heartbeat for non-sensitive refresh events |
| `/app/events` | SSE fallback heartbeat for non-sensitive refresh events           |
| `/app/fragments/*` | customer-safe HTML views; cluster-wide data stays hidden    |
| `/api/customer/*` | authenticated, Postgres-backed customer data APIs             |
| `/_customer/*` | customer portal Vite assets (`CUSTOMER_STATIC_DIR`)             |
| everything else | the static [Astro](https://astro.build) site (`STATIC_DIR`)     |

The frontend is the sibling [`fiducia-ui.web`](https://github.com/fiducia-cloud/fiducia-ui.web)
repo. It is **not** committed here — the deployment builds it in-pod and points
this backend at the result via `STATIC_DIR`.

The customer portal assets are the sibling
[`fiducia-customer-ui.web`](https://github.com/fiducia-cloud/fiducia-customer-ui.web)
repo. They are also **not** committed here; build them and point this backend at
the result via `CUSTOMER_STATIC_DIR`. Requests with `Host: app.fiducia.cloud`
serve the customer portal at `/`; `/app` always serves it. Set
`FIDUCIA_SITE_MODE=customer` if a dedicated deployment should render the portal
at `/` regardless of host.

## Run locally

```bash
# Build the frontends somewhere and point at them:
STATIC_DIR=../fiducia-ui.web/dist \
CUSTOMER_STATIC_DIR=../fiducia-customer-ui.web/dist \
DATABASE_URL=postgres://... \
cargo run   # listens on :8080 (override PORT)
```

`STATIC_DIR` defaults to `static`. Files are served from its root; the backend
does not add a path prefix (the gateway strips `/fiducia/` before requests
arrive — the Astro build carries the `/fiducia` base so asset URLs round-trip).
`CUSTOMER_STATIC_DIR` defaults to `customer-static`. If `SUPABASE_URL` and
`SUPABASE_ANON_KEY` are set, the rendered portal passes them to the browser for
Supabase realtime subscriptions.

`DATABASE_URL` is required. The service refuses to start without durable
customer Postgres. Customer preferences, sessions, API keys, and sync
idempotency are persisted; dependency failures return explicit errors instead
of invented successes or sample rows.

API-key rotation replaces the old secret immediately (`overlap_seconds=0`);
deploy callers accordingly. The portal can display locally observed session
records, but provider-backed session revocation is intentionally reported as
unsupported until Supabase session identifiers and Admin revocation are wired.
TOTP enrollment is available in the UI, but production-key issuance is not yet
gated on AAL2; do not treat enrollment as an enforced issuance policy. Privileged
admin scopes are not issued by this customer-membership-only API.

The customer browser keeps one tenant-filtered Supabase realtime WebSocket and
one backend heartbeat stream. The heartbeat prefers `/app/ws` and falls back to
`/app/events`; it carries only generic refresh frames and never customer rows,
API-key metadata, or credentials. Durable customer changes are loaded through
authenticated, tenant-scoped catch-up APIs or Supabase RLS subscriptions.
The portal does not expose `fiducia-node`'s cluster-wide observability APIs as
customer data. Those panels explicitly remain unavailable until the node has an
authenticated tenant-scoped read contract.

## Configuration

All configuration is read from the environment. Defaults are secure-by-default:
an unset/unknown `FIDUCIA_SITE_MODE` uses host-based routing (not the permissive
"customer" mode), and an unset `FIDUCIA_AUTH_URL` makes the customer APIs fail
closed (`Deny`).

| Var | Type | Secret? | Meaning | Default |
|-----|------|---------|---------|---------|
| `PORT` | integer | no | TCP port to listen on. | `8080` |
| `STATIC_DIR` | string (dir) | no | Directory of the built Astro marketing site. | `static` |
| `CUSTOMER_STATIC_DIR` | string (dir) | no | Directory of the built customer portal assets. | `customer-static` |
| `CUSTOMER_APP_HOST` | string (host) | no | Host that serves the customer portal at `/`. | `app.fiducia.cloud` |
| `FIDUCIA_SITE_MODE` | string (mode) | no | `customer` renders the portal at `/` regardless of host. Any other/unset value uses the **safe** host-based routing (portal only at `/app` or for `CUSTOMER_APP_HOST`). | unset → host-based (safe) |
| `FIDUCIA_AUTH_URL` | string (URL) | no | Base URL of `fiducia-auth`; verifies customer Supabase sessions. **Unset → fail closed**: every `/api/customer/*` route denies. | unset → `Deny` |
| `SUPABASE_URL` | string (URL) | no | Supabase project URL handed to the browser for realtime. | unset |
| `SUPABASE_ANON_KEY` | string (key) | no (anon/public key) | Supabase **anon (public)** key handed to the browser for realtime. Not a service-role secret. | unset |
| `DATABASE_URL` | string (URL) | **yes** (DB credentials) | Customer Postgres. **Required** — the service refuses to start without it. | none (required) |
| `TEST_DATABASE_URL` | string (URL) | **yes** (DB credentials) | Postgres the test harness may create/drop freely; gates the store integration tests (unset → those tests skip). | unset |

`FIDUCIA_E2E_STATIC_CUSTOMER_AUTH=1` forces a fixed test identity, but only in
**debug** builds — it is impossible in release binaries, so production stays
fail-closed even if the variable leaks into the environment.

### CLI flags (`flags-2-env`)

The pinned [`flags-2-env`](https://github.com/ORESoftware/flags-2-env) submodule
(`vendor/flags-2-env`) maps CLI flags to the env vars above via the
`.cli-flags.toml` schema. Build the parser once with
`make -C vendor/flags-2-env all`, then run through `scripts/with-flags2env.sh`:

```bash
scripts/with-flags2env.sh --port 8080 --static-dir ../fiducia-ui.web/dist -- cargo run
```

`TEST_DATABASE_URL` is marked sensitive in the schema. CI audits the schema
(`.github/workflows/cli-flags.yml`).

## Deployment

Built and run in-cluster on both the AWS and Hetzner Kubernetes clusters behind
the shared gateway under `/fiducia/`, mirroring `canonical.cloud`:

1. a **node initContainer** clones `fiducia-ui.web`, runs `astro build --base /fiducia`, and writes `dist/` to a shared volume;
2. a **node initContainer** clones `fiducia-customer-ui.web`, runs `npm run build`, and writes `dist/` to a shared volume;
3. this **rust container** clones `fiducia-backend.rs`, `cargo run --release`, and serves those volumes via `STATIC_DIR` and `CUSTOMER_STATIC_DIR`.

Manifests live in [`ORESoftware/k8s-cluster`](https://github.com/ORESoftware/k8s-cluster)
at `remote/argocd/dd-next-runtime/dd-fiducia-rs.*`; this repo is wired in as the
`remote/deployments/fiducia-backend.rs` git submodule.

## Security

**Secure-by-default posture.** The customer authenticator is fail-closed
(`Authenticator::Deny` when `FIDUCIA_AUTH_URL` is unset), so `/api/customer/*`
never serves data without a verified Supabase session; writes are scoped to the
caller's org (never "first org"). `FIDUCIA_SITE_MODE` defaults to the restricted
host-based routing — the permissive `customer` mode must be set explicitly.
There is no `FIDUCIA_ALLOW_INSECURE_*`/dev-session toggle; the only test-auth
escape hatch (`FIDUCIA_E2E_STATIC_CUSTOMER_AUTH`) is compiled out of release
builds.

**Hardening in place.** All SQL is parameterized (`sqlx` `.bind`, no string
concatenation). The middleware stack sets `X-Content-Type-Options: nosniff`,
`X-Frame-Options: DENY`, a referrer policy, a permissions policy, and a CSP; it
bounds request time (`TimeoutLayer`, 30s), caps bodies (`RequestBodyLimitLayer`,
64 KiB), and catches handler panics (`CatchPanicLayer`). API-key secrets use the
OS CSPRNG and only their hash is persisted. There is no permissive CORS layer.

**Heartbeat no longer fans out customer rows.** A process-wide broadcast channel
that placed `api_keys` change frames onto the public `/app/ws` + `/app/events`
portal heartbeat has been removed. The heartbeat now carries only generic
refresh frames; durable customer changes are loaded through authenticated,
tenant-scoped catch-up APIs or Supabase RLS subscriptions.

**Accepted advisories** (no clean in-semver fix; recorded rather than force-fixed):

- `rsa` [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071) —
  Marvin timing side-channel. Transitive (via `sqlx`'s MySQL path, unused here);
  no fixed upgrade is published.
- `proc-macro-error` [RUSTSEC-2024-0370](https://rustsec.org/advisories/RUSTSEC-2024-0370)
  and `proc-macro-error2` [RUSTSEC-2026-0173](https://rustsec.org/advisories/RUSTSEC-2026-0173)
  — unmaintained. Build-time proc-macro deps only; no runtime exposure.

Run `cargo audit` to re-check. These three are the only known findings.
