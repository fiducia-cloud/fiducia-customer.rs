# src

The entire Rust backend. This crate is a **bin-only** crate (no `lib.rs`); all
code lives in `main.rs`.

- **`main.rs`** — builds the axum `Router` and runs it. It wires:
  - health/info probes (`/healthz`, `/api/health`, `/api/info`);
  - the customer portal, rendered server-side with Maud and refreshed with HTMX
    (`/app`, `/app/*`), plus its `/app/ws` WebSocket and `/app/events` SSE
    streams that push refresh events and `fiducia:sync` change frames;
  - Postgres-backed API keys, users, preferences, sessions, and the durable
    `@fiducia/sync` write/idempotency path (`/api/customer/...`);
  - customer-safe coordination fragments that explicitly withhold cluster-wide
    locks, metrics, KV, and discovery data until a tenant-scoped API exists;
  - a static-file fallback that serves the built Astro site (`STATIC_DIR`).

`build_router()` is intentionally split from `main()` so unit tests can exercise
routes without binding a socket. Production startup requires `DATABASE_URL`;
tests construct missing-dependency states only to prove handlers fail closed.
