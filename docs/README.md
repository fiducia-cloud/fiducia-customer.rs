# docs

Design and architecture documentation for fiducia-backend.

- **`diagram.html`** — a self-contained Mermaid architecture diagram, rendered
  client-side and also served by the app at `/docs/diagram`.

Credential storage and `require_idempotency` policy are documented in
`fiducia-auth.rs`; this BFF only forwards the authenticated, sanitized contract.
