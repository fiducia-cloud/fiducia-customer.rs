# .github/workflows

GitHub Actions pipelines for this service.

- **`ci.yml`** — on push/PR to `main`: `cargo fmt --check`, `clippy`, `cargo test
  --bins` (the gating step — bin-only crate), and `cargo audit`. It also checks
  out the sibling `fiducia-cloud/fiducia-interfaces` repo alongside so the
  path-dependency crates (`../fiducia-interfaces/generated/...`) resolve.
- **`deploy-test.yml`** — secret-gated rollout to the `fiducia-test` Kubernetes
  namespace (sets the deployment image to the commit-SHA tag). No-op when
  `KUBE_CONFIG_TEST` is absent; PROD deploys happen from the fiducia-monorepo,
  not here.
