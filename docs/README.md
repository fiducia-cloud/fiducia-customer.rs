# docs

Design and architecture documentation for fiducia-backend.

- **`diagram.html`** — a self-contained Mermaid architecture diagram, rendered
  client-side and also served by the app at `/docs/diagram`.
- **`require-idempotency-wiring.md`** — the end-to-end plan for persisting and
  propagating the per-key `require_idempotency` flag (backend + customer DB +
  edge), the last mile of a mechanism already built across `fiducia-interfaces`,
  `fiducia-auth`, and `fiducia-load-balance`. Pairs with
  `scripts/2026-07-add-require-idempotency.sql`.
