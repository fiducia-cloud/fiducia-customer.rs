# .nix

Nix flake defining the reproducible development shell for this repo. The root
`.envrc` (`use flake ./.nix`) and the `./shell` helper both enter it.

- **`flake.nix`** — a multi-system `devShell` (Linux/macOS, x86_64/aarch64)
  providing the Rust toolchain (rustc, cargo, rustfmt, clippy, rust-analyzer) plus
  git, direnv, just, bacon, node/pnpm, and pkg-config/openssl.
- **`flake.lock`** — pinned input revisions (do not edit by hand).
