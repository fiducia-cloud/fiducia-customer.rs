# fiducia-backend

The canonical **customer web application and BFF** for
[fiducia.cloud](https://fiducia.cloud). It is a Rust deployment built with the
MASH stack:

- **Maud** renders compile-checked, escaped HTML.
- **Axum** owns HTTP routes, middleware, cookies, WebSocket/SSE endpoints, and
  server-mediated login.
- **SeaORM** owns the customer Postgres connection and application CRUD.
- **HTMX** progressively enhances same-origin forms and authenticated fragments.

The static sibling `fiducia-ui.web` is the Astro **marketing site only**. Its
build is served as the fallback for the public host. The customer application is
rendered here; it no longer depends on `fiducia-customer-ui.web` or a Vite SPA.

## Hard boundary from admin

The customer app and `fiducia-admin.rs` are separate deployments and security
planes.

| Boundary | Customer | Admin |
|---|---|---|
| Repository | `fiducia-backend.rs` | `fiducia-admin.rs` |
| Cookie | `fiducia_customer_session` | `fiducia_admin_session` |
| Database | `fiducia-interfaces/sql/customer.sql` | `fiducia-interfaces/sql/admin.sql` |
| Authorization | verified user plus org membership | trusted operator role plus local operator registry |
| Routes | `/app`, `/api/customer/*` | `/`, `/infra`, `/api/admin/*` |

Neither Rust server reads the other database or accepts the other cookie.

## Authentication

`POST /login` sends the email/password exchange from the Rust server to
Supabase Auth. The returned access token is immediately verified through
`fiducia-auth GET /v1/me`; a customer session is issued only when that verified
identity has at least one trusted organization claim.

The token then rides in an `HttpOnly; SameSite=Strict; Secure`
`fiducia_customer_session` cookie. API clients may use the same Supabase token
as `Authorization: Bearer ...`. Browser JavaScript never receives a service-role
key or an application session token.

API-key creation and rotation are delegated to `fiducia-auth`, which writes the
authoritative verifier into Fiducia KV for edge/load-balancer introspection. This
server mirrors the sanitized relational metadata into customer Postgres through
SeaORM. If that mirror fails, it revokes the newly created/rotated authoritative
key so a live orphan credential is not left behind.

## Routes

| Route | Purpose |
|---|---|
| `GET/POST /login` | server-mediated Supabase sign-in |
| `POST /logout` | clear only the customer cookie |
| `GET /app/*` | authenticated Maud customer pages |
| `GET /app/fragments/*` | authenticated HTMX fragments |
| `POST /app/api-keys` | HTMX API-key creation; plaintext shown once |
| `POST /app/settings` | HTMX preference persistence |
| `POST /app/security/sessions/revoke` | HTMX trusted-session revocation |
| `GET/POST /api/customer/*` | authenticated JSON customer API |
| `GET /app/ws`, `GET /app/events` | authenticated refresh channels |
| `GET /healthz`, `GET /api/health` | health probes |
| `GET /api/info` | deployment metadata |
| other paths | static `fiducia-ui.web` marketing build |

The lock, request, KV, and discovery panels deliberately report unavailable
until `fiducia-node.rs` exposes a tenant-scoped observability API. The customer
app never invents cluster rows or exposes cluster-wide data.

## Database

`DATABASE_URL` is required. Startup fails when the customer database is absent
or unreachable. Production access goes through one SeaORM
`DatabaseConnection`; Postgres-specific atomic idempotency claims use
SeaORM-bound statements. SQLx is test-only for applying and seeding the canonical
schema.

The schema source of truth is
[`fiducia-interfaces/sql/customer.sql`](../fiducia-interfaces/sql/customer.sql).
It defines organizations, projects, users, memberships, API keys, preferences,
trusted sessions, audit, RLS, realtime publication boundaries, and the durable
sync idempotency ledger.

## Run locally

```sh
STATIC_DIR=../fiducia-ui.web/dist \
DATABASE_URL=postgres://... \
FIDUCIA_AUTH_URL=http://127.0.0.1:8081 \
SUPABASE_URL=https://example.supabase.co \
SUPABASE_PUBLISHABLE_KEY=public-key \
cargo run
```

The server listens on `:8080` by default. Set
`FIDUCIA_INSECURE_COOKIES=1` only for local plain-HTTP development.

Non-secret options can be mapped from flags through the pinned
`flags-2-env` helper:

```sh
make -B -C vendor/flags-2-env all
scripts/with-flags2env.sh --port 8080 --static-dir ../fiducia-ui.web/dist -- cargo run --locked
```

## Configuration

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | required customer Postgres credentials |
| `FIDUCIA_AUTH_URL` | required `fiducia-auth` base URL |
| `SUPABASE_URL` | required Supabase project URL |
| `SUPABASE_PUBLISHABLE_KEY` | required public key for the server-mediated exchange |
| `STATIC_DIR` | Astro marketing build; default `static` |
| `CUSTOMER_APP_HOST` | customer host; default `app.fiducia.cloud` |
| `FIDUCIA_SITE_MODE=customer` | render the customer app at `/` regardless of Host |
| `PORT` | listen port; default `8080` |
| `FIDUCIA_INSECURE_COOKIES=1` | local-only escape hatch removing `Secure` |
| `TEST_DATABASE_URL` | opt-in real-Postgres behavior tests |

`FIDUCIA_E2E_STATIC_CUSTOMER_AUTH=1` exists only in debug builds. Release
binaries remain fail-closed.

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
vendor/flags-2-env/build/flags2env audit .cli-flags.toml
git diff --check
```

The DB behavior tests cover tenant isolation, API-key rotation/revocation,
durable idempotency replay, user provisioning, preferences, and trusted-session
revocation against the canonical Postgres schema.

<!-- BEGIN k8s-cluster-submodule-notice -->
> [!NOTE]
> **Canonical source.** This repository is the source of truth for its code. It
> is also vendored as a secondary git submodule of
> [ORESoftware/k8s-cluster](https://github.com/ORESoftware/k8s-cluster) at
> `remote/deployments/fiducia-backend.rs`; make changes here, not in that
> submodule checkout.
<!-- END k8s-cluster-submodule-notice -->
