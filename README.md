# fiducia-backend

Rust + [axum](https://github.com/tokio-rs/axum) backend for **fiducia.cloud** —
"consensus & coordination as a service" (Raft-based mutual-exclusion locks, rate
limiting, and cron).

It serves two things:

| Path        | Served                                                              |
|-------------|---------------------------------------------------------------------|
| `/api/*`    | JSON API (`/api/health`, `/api/info`)                               |
| everything else | The static [Astro](https://astro.build) site from `static/`     |

## Layout

```
src/main.rs     # axum server: /api/* + ServeDir(static/)
static/         # COMMITTED build of ../fiducia-ui.web (the homepage)
```

`static/` is the output of the sibling `fiducia-ui.web` repo. Regenerate it with
`npm run sync` over there. It is committed so the deployment pod (which compiles
only the Rust binary) has the frontend assets without needing Node.

## Run locally

```bash
cargo run                      # listens on :8080 (override with PORT)
# STATIC_DIR lets you point at the frontend dist directly during dev:
STATIC_DIR=../fiducia-ui.web/dist cargo run
```

## Deployment

Built and run in-cluster from source (`cargo run --release`) on both the AWS and
Hetzner Kubernetes clusters, behind the shared gateway under `/fiducia/`. The
manifests live in
[`ORESoftware/k8s-cluster`](https://github.com/ORESoftware/k8s-cluster) at
`remote/argocd/dd-next-runtime/dd-fiducia-rs.*`, where this repo is wired in as
the `remote/deployments/fiducia-backend.rs` git submodule.
