<!-- generated-policy: frozen -->

# `generated/` — committed, and not hand-editable

Everything in this directory is machine-written and **committed to version
control**. Do not edit these files by hand. Change the source they come from
and re-run the generator.

Typical producers:

- [`flags-2-env`](https://github.com/flags-2-env/flags-2-env) — e.g. `generated/dart/env.dart`
- [`oresoftware/api-docs`](https://github.com/oresoftware/api-docs) — route maps and clients
- JSON Schema / OpenAPI / route-map generators in this repository

## In this repository

The api-docs generator (`remote/tools/generate-api-docs.mjs`) scans the router's
flat route declarations in `src/main.rs`, per the "API Docs Contract" in
`AGENTS.md`, and writes:

- **`api-docs.json`** — machine-readable route/endpoint description.
- **`api-docs.html`** — human-readable rendering of the same.

The app serves them at `/docs/api`, `/api/docs`, and `/api/docs.json`.

## Why the files are read-only on disk

After generation they are frozen with `chmod a-w`. Your editor will refuse the
write, which is the point — it turns "I edited the wrong file" into an error you
see immediately rather than a diff you notice in review. Directories and this
`README.md` stay writable so generators can replace files.

**Git does not store this.** Git tracks only the executable bit, so every file
here is mode `100644` in the object database and a fresh clone comes back
writable. The read-only bit is a local ergonomic guard; it is *not* what
enforces the policy.

## What actually enforces the policy

CI, not the filesystem:

| Guard | Where | What it catches |
| --- | --- | --- |
| `check-generated-contract.py` | CI + pre-commit | a hand-edited or thawed file |
| regenerate-and-diff | CI | committed output that no longer matches its source |
| `post-checkout` / `post-merge` hooks | your clone | re-freezes after every checkout |

Enable the hooks once per clone:

```sh
git config core.hooksPath .githooks
```

Re-freeze at any time (safe, idempotent):

```sh
python3 scripts/check-generated-contract.py --freeze --require-readonly
```

Without the checker on hand, the equivalent one-liner is:

```sh
find generated -type f ! -name 'README.md' ! -name 'readme.md' -exec chmod a-w {} +
```

## Regenerating

Edit the **primary source** — `.cli-flags.toml`, the route map, `*.schema.json`
— then run the generator. Generators thaw, write, and re-freeze on their own. If
you are committing a regeneration, the pre-commit guard needs to be told so:

```sh
REGEN=1 git commit -m "Regenerate clients from the updated route map"
```

## Gitignored trees

If a `generated/` tree is in `.gitignore`, its artifacts stay off VCS — that is the
`ignored` policy rather than `frozen`. Still commit the tree's `README.md`
(`git add -f generated/README.md` or a `.gitignore` exception) so the policy is
visible. Example exception:

```
generated/**
!generated/README.md
```

## Runtime contract (not just compile-time)

JSON Schema is a **cross-check**, not the codegen input. The generated types come
from the primary IR (route map, `.cli-flags.toml`); the schema is an independently
derived description of the same contract, so disagreement means one of them has
drifted. Unit tests should validate fixtures/examples against Draft 2020-12 at
runtime (valid must pass, invalid must fail) and compare schema keys to
`.cli-flags.toml` env names or route-map keys when those exist.
