# scripts

The customer BFF does not seed or migrate API-key rows: `fiducia-auth` owns the
entire credential lifecycle and durable key policy.

- **`with-flags2env.sh`** — bridges CLI flags to the environment variables this
    backend reads (`PORT`, marketing `STATIC_DIR`, `FIDUCIA_*`, and public
    `SUPABASE_*` settings). Database credentials and debug auth remain
    environment-only. It runs the pinned `flags2env` parser
  (`vendor/flags-2-env`) against the `.cli-flags.toml` schema, exports the
  resulting env map, then execs the given command
  (e.g. `scripts/with-flags2env.sh --port 8080 --static-dir ../fiducia-ui.web/dist -- cargo run`).
  Build the parser first with `make -B -C vendor/flags-2-env all`.
