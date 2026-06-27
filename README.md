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
| `/app/ws` | customer portal WebSocket stream for rendered dashboard fragments    |
| `/app/events` | SSE fallback stream for rendered dashboard fragments             |
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
cargo run   # listens on :8080 (override PORT)
```

`STATIC_DIR` defaults to `static`. Files are served from its root; the backend
does not add a path prefix (the gateway strips `/fiducia/` before requests
arrive — the Astro build carries the `/fiducia` base so asset URLs round-trip).
`CUSTOMER_STATIC_DIR` defaults to `customer-static`. If `SUPABASE_URL` and
`SUPABASE_ANON_KEY` are set, the rendered portal passes them to the browser for
Supabase realtime subscriptions.

The customer browser keeps one Supabase realtime WebSocket and one backend
stream. The backend stream prefers `/app/ws` and falls back to `/app/events`;
both send rendered HTML fragments for the dashboard panels so normal stream
updates do not need a new HTMX HTTP request per fragment.

## Deployment

Built and run in-cluster on both the AWS and Hetzner Kubernetes clusters behind
the shared gateway under `/fiducia/`, mirroring `canonical.cloud`:

1. a **node initContainer** clones `fiducia-ui.web`, runs `astro build --base /fiducia`, and writes `dist/` to a shared volume;
2. a **node initContainer** clones `fiducia-customer-ui.web`, runs `npm run build`, and writes `dist/` to a shared volume;
3. this **rust container** clones `fiducia-backend.rs`, `cargo run --release`, and serves those volumes via `STATIC_DIR` and `CUSTOMER_STATIC_DIR`.

Manifests live in [`ORESoftware/k8s-cluster`](https://github.com/ORESoftware/k8s-cluster)
at `remote/argocd/dd-next-runtime/dd-fiducia-rs.*`; this repo is wired in as the
`remote/deployments/fiducia-backend.rs` git submodule.
