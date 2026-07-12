# src

The entire Rust backend. This crate is a **bin-only** crate (no `lib.rs`); all
code lives in `main.rs`.

- **`main.rs`** — builds the axum `Router` and runs it. It wires:
  - health/info probes (`/healthz`, `/api/health`, `/api/info`);
  - the customer portal, rendered server-side with Maud and refreshed with HTMX
    (`/app`, `/app/*`), plus its `/app/ws` WebSocket and `/app/events` SSE
    streams that push rendered dashboard fragments and `fiducia:sync` change
    frames;
  - the DB-backed `api_keys` vertical and the `@fiducia/sync` write path
    (`/api/customer/...`), served from the customer Postgres plane when
    `DATABASE_URL` is set and degrading to in-memory mocks otherwise;
  - a static-file fallback that serves the built Astro site (`STATIC_DIR`).

`build_router()` is intentionally split from `main()` so the unit tests (run via
`cargo test --bins`) can exercise routes without binding a socket. The dashboard
data (`locks()`, `requests()`, `kv_entries()`, `services()`, …) is mock/demo data
— this tier does not implement coordination; that lives in `fiducia-node.rs` and
`fiducia-brain.rs`.
