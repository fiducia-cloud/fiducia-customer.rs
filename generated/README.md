<!-- generated-policy: frozen -->

# generated

Generated API documentation — do **not** edit by hand. These files are produced
by the shared api-docs generator (`remote/tools/generate-api-docs.mjs`, which
scans the router's flat route declarations in `src/main.rs`) per the "API Docs
Contract" in `AGENTS.md`. The app serves them at `/docs/api`, `/api/docs`, and
`/api/docs.json`.

- **`api-docs.json`** — machine-readable route/endpoint description.
- **`api-docs.html`** — human-readable rendering of the same.

Regenerate them from the source of truth (the router) rather than editing here.

## Read-only + runtime contract

<!-- generated-policy: frozen -->

Files in this `generated/` tree are frozen with `chmod a-w` after generation.
This README stays writable. Git does not persist the write bit; re-run:

```sh
python3 scripts/check-generated-contract.py --freeze --require-readonly
```

JSON Schema is a runtime **cross-check** (fixtures must validate; invalid
fixtures must fail). Compile-time types alone are not the contract.
