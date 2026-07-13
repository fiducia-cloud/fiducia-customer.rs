# fiducia-backend

The canonical customer web application and BFF for
[fiducia.cloud](https://fiducia.cloud). It is a Rust MASH deployment:

- **Maud** renders escaped, compile-checked customer HTML.
- **Axum** owns routes, middleware, cookies, WebSocket/SSE refresh endpoints,
  and server-mediated login.
- **SeaORM** owns customer profile, preference, and local session persistence in
  Postgres.
- **HTMX** progressively enhances same-origin forms and authenticated fragments.

The sibling `fiducia-ui.web` is the static Astro marketing site only; its build
is the fallback for the public host. The deprecated `fiducia-customer-ui.web`
SPA is preserved for history but is not loaded or deployed by this service.

## Customer and admin are separate

| Boundary | Customer | Admin |
|---|---|---|
| Repository | `fiducia-backend.rs` | `fiducia-admin.rs` |
| Cookie | `fiducia_customer_session` | `fiducia_admin_session` |
| Database | `fiducia-interfaces/sql/customer.sql` | `fiducia-interfaces/sql/admin.sql` |
| Authorization | verified user plus explicit org membership | trusted operator role plus local operator registry |
| Routes | `/app`, `/api/customer/*` | `/`, `/infra`, `/api/admin/*` |

Neither server reads the other database or accepts the other cookie. Cluster
locks, requests, KV, and service-discovery controls belong only to the admin
application; the customer portal does not expose cluster-wide operator data.

## Authentication and credentials

`POST /login` performs the email/password exchange from this Rust server to
Supabase Auth. The returned access token is verified through
`fiducia-auth GET /v1/me`; a customer cookie is issued only for a verified user
with trusted organization membership.

The token is stored in an `HttpOnly; SameSite=Strict; Secure`
`fiducia_customer_session` cookie. API clients may instead send the same token as
`Authorization: Bearer ...`. Browser JavaScript never receives a service-role
key or the application session token.

`fiducia-auth` is the sole API-key authority. This BFF authenticates the customer,
requires an explicit verified organization for multi-org accounts, forwards
create/list/rotate/revoke operations and mutation `Idempotency-Key` values, and
returns only the typed sanitized display contract. It does not mint credentials,
store verifier hashes, or maintain a second credential database. Secret-bearing
responses are marked `Cache-Control: no-store`; exact retries are replay-safe in
the auth service.

## Routes

| Route | Purpose |
|---|---|
| `GET/POST /login` | server-mediated Supabase sign-in |
| `POST /logout` | clear only the customer cookie |
| `GET /app/*` | authenticated Maud customer pages |
| `GET /app/fragments/*` | authenticated HTMX fragments |
| `POST /app/api-keys` | replay-safe HTMX API-key creation; plaintext shown once |
| `POST /app/settings` | SeaORM preference persistence |
| `POST /app/security/sessions/revoke` | user-scoped local session audit update |
| `GET/POST /api/customer/*` | authenticated JSON customer BFF |
| `GET /app/ws`, `GET /app/events` | authenticated non-sensitive refresh heartbeat |
| `GET /healthz`, `GET /api/health` | health probes |
| `GET /api/info` | deployment metadata |
| other paths | static `fiducia-ui.web` marketing build |

The heartbeat transports refresh signals and server-rendered summary fragments,
not customer rows, API-key metadata, or credentials. Customer data is reloaded
through authenticated, tenant-scoped routes.

## Database

`DATABASE_URL` is required and startup fails if Postgres is unavailable.
Production persistence uses one SeaORM `DatabaseConnection`. Supabase remains
the identity source of truth; SeaORM provisions the local user row and persists
customer preferences and user-scoped local session observations. Marking a local
session revoked is an audit-state change, not yet provider-backed Supabase token
revocation.

The schema source of truth is
[`fiducia-interfaces/sql/customer.sql`](../fiducia-interfaces/sql/customer.sql).
Real Postgres tests use `TEST_DATABASE_URL` when supplied and otherwise skip
without inventing database state.

## Run locally

```sh
STATIC_DIR=../fiducia-ui.web/dist \
DATABASE_URL=postgres://... \
FIDUCIA_AUTH_URL=http://127.0.0.1:8097 \
SUPABASE_URL=https://example.supabase.co \
SUPABASE_PUBLISHABLE_KEY=public-key \
FIDUCIA_INSECURE_COOKIES=1 \
cargo run --locked
```

The server listens on `:8080` by default. `FIDUCIA_INSECURE_COOKIES=1` is only
for local plain-HTTP development.

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | required customer Postgres credentials |
| `FIDUCIA_AUTH_URL` | required `fiducia-auth` base URL |
| `SUPABASE_URL` | required Supabase project URL |
| `SUPABASE_PUBLISHABLE_KEY` | required public key for server-mediated sign-in |
| `STATIC_DIR` | Astro marketing build; default `static` |
| `CUSTOMER_APP_HOST` | customer host; default `app.fiducia.cloud` |
| `CUSTOMER_APP_ORIGIN` | optional exact cross-origin browser allowlist; unset is same-origin only |
| `FIDUCIA_SITE_MODE=customer` | render the customer app at `/` regardless of Host |
| `PORT` | listen port; default `8080` |
| `FIDUCIA_INSECURE_COOKIES=1` | local-only escape hatch removing `Secure` |
| `TEST_DATABASE_URL` | opt-in real-Postgres behavior tests |

`FIDUCIA_E2E_STATIC_CUSTOMER_AUTH=1` exists only in debug builds. Release
binaries remain fail-closed.

Non-secret options can be mapped from audited flags:

```sh
make -B -C vendor/flags-2-env all
scripts/with-flags2env.sh --port 8080 --static-dir ../fiducia-ui.web/dist -- cargo run --locked
```

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit
vendor/flags-2-env/build/flags2env audit .cli-flags.toml
git diff --check
```

<!-- BEGIN k8s-cluster-submodule-notice -->
> [!NOTE]
> **Canonical source.** This repository is the source of truth for its code. It
> is also vendored as a secondary git submodule of
> [ORESoftware/k8s-cluster](https://github.com/ORESoftware/k8s-cluster) at
> `remote/deployments/fiducia-backend.rs`; make changes here, not in that
> submodule checkout.
<!-- END k8s-cluster-submodule-notice -->
