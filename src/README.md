# src

The customer MASH server is a bin-only crate (no `lib.rs`).

- **`main.rs`** — builds the axum `Router` and runs it. It wires:
  - health/info probes (`/healthz`, `/api/health`, `/api/info`);
  - server-mediated Supabase login/logout and the isolated customer cookie;
  - the customer portal, rendered server-side with Maud and refreshed with the
    vendored HTMX bundle (`/app`, `/app/*`), plus authenticated WebSocket/SSE
    endpoints that push non-sensitive refresh events;
  - Postgres-backed users, preferences, and user-scoped session observations;
  - the authenticated, organization-scoped customer BFF for API-key
    list/create/rotate/revoke and read-only `@fiducia/sync` hydration; key
    authority and idempotent lifecycle state remain in `fiducia-auth`;
  - no cluster-wide locks, metrics, KV, or discovery routes; those controls
    belong only to `fiducia-admin.rs`;
- **`auth.rs`** — accepts the customer cookie or bearer token and delegates
  verification to `fiducia-auth`; admin cookies are deliberately ignored.
- **`store.rs`** — SeaORM-owned customer profile, preference, and local-session
  CRUD over the canonical `fiducia-interfaces` schema. Credential storage is
  intentionally absent.
- **`entity/`** — SeaORM models for customer tables.

The static-file fallback serves only the built Astro marketing site
(`fiducia-ui.web` via `STATIC_DIR`). Customer assets are compiled into the Rust
binary from `assets/`; there is no runtime customer-SPA dependency.

`build_router()` is intentionally split from `main()` so unit tests can exercise
routes without binding a socket. Production startup requires `DATABASE_URL`;
tests construct missing-dependency states only to prove handlers fail closed.
