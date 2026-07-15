# .github/workflows

GitHub Actions pipelines for this service.

- **`ci.yml`** — on push/PR to `main`: mandatory full-workspace formatting,
  locked all-target/all-feature Clippy and tests, and a pinned `cargo-audit`.
  It checks out the sibling
  `fiducia-cloud/fiducia-interfaces` repo at the exact commit also pinned by the
  Dockerfile so the path-dependency crates
  (`../fiducia-interfaces/generated/...`) resolve reproducibly.

This repository is validation-only: it does not receive kubeconfig credentials
or deploy itself. Fleet deployment is owned by `fiducia-monorepo`.

## Security baseline

Every executable workflow uses explicit least-privilege permissions, immutable
third-party action or container references, non-persisted checkout credentials,
concurrency control, and a job timeout. The main CI workflow validates this
directory with the digest-pinned actionlint container. Environment mutation is
forbidden unless this README documents a repository-specific platform exception.
