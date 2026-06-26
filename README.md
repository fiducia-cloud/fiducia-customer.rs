# fiducia-backend

Rust + [axum](https://github.com/tokio-rs/axum) backend for **fiducia.cloud** —
"consensus & coordination as a service" (Raft-based mutual-exclusion locks, rate
limiting, and cron).

It serves two things:

| Path        | Served                                                              |
|-------------|---------------------------------------------------------------------|
| `/healthz`, `/api/health` | health probe                                          |
| `/api/info` | service / version JSON                                              |
| everything else | the static [Astro](https://astro.build) site (`STATIC_DIR`)     |

The frontend is the sibling [`fiducia-ui.web`](https://github.com/fiducia-cloud/fiducia-ui.web)
repo. It is **not** committed here — the deployment builds it in-pod and points
this backend at the result via `STATIC_DIR`.

## Run locally

```bash
# Build the frontend somewhere and point at it:
STATIC_DIR=../fiducia-ui.web/dist cargo run   # listens on :8080 (override PORT)
```

`STATIC_DIR` defaults to `static`. Files are served from its root; the backend
does not add a path prefix (the gateway strips `/fiducia/` before requests
arrive — the Astro build carries the `/fiducia` base so asset URLs round-trip).

## Deployment

Built and run in-cluster on both the AWS and Hetzner Kubernetes clusters behind
the shared gateway under `/fiducia/`, mirroring `canonical.cloud`:

1. a **node initContainer** clones `fiducia-ui.web`, runs `astro build --base /fiducia`, and writes `dist/` to a shared volume;
2. this **rust container** clones `fiducia-backend.rs`, `cargo run --release`, and serves that volume via `STATIC_DIR`.

Manifests live in [`ORESoftware/k8s-cluster`](https://github.com/ORESoftware/k8s-cluster)
at `remote/argocd/dd-next-runtime/dd-fiducia-rs.*`; this repo is wired in as the
`remote/deployments/fiducia-backend.rs` git submodule.
