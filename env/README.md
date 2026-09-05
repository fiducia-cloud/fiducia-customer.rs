# Encrypted runtime configuration

This service has one reviewed secret contract:

```text
env/enc/dev.env.enc    # SOPS + age ciphertext; may be committed
env/enc/prod.env.enc   # SOPS + age ciphertext; may be committed
env/dec/dev.env        # ignored local plaintext, mode 0600
env/dec/prod.env       # ignored local plaintext, mode 0600
.env -> env/dec/<dev|prod>.env
```

There are exactly two tracked secret stores: `dev` and `prod`. Do not create
`local`, `staging`, `production`, per-person, or per-provider ciphertext files.
Deployment platforms may bind different values, but they must implement the
same documented variable-name contract.

`env/dec/` is a disposable runtime boundary. It is never authoritative and must
be created only through `ores-sops ensure-dec`, which rejects symlink and
non-directory redirection and applies restrictive permissions.

## Canonical workflow

```sh
ores-sops verify        # keyless policy audit; safe for pull-request CI
ores-sops edit dev      # edit through SOPS; plaintext does not persist
ores-sops edit prod
ores-sops use dev       # atomically materialize and activate the managed .env
ores-sops use prod
ores-sops diff dev      # variable names only; never values
ores-sops status
ores-sops lock          # remove managed plaintext and the managed .env link
```

`just env-verify` is the repository alias for the keyless audit. Pull-request CI
has no age identity and must not decrypt values. Trusted release/runtime jobs
may separately prove decryptability through protected workload identity.

## Application configuration contract

The application owns variable names, types, required/optional status, safe
defaults, and precedence. Deployment owns values. Runtime configuration must be
parsed and validated once at process startup into an immutable typed value;
malformed or missing required values fail before the listener starts. Do not
read `std::env::var` throughout request handlers or hide deploy-specific values
in source constants.

Non-secret settings may be supplied through flags, ConfigMaps, or platform
configuration. Credentials, signing/encryption material, database URLs carrying
credentials, provider tokens, and CSRF/HMAC material belong in the encrypted
store or platform secret manager. `.cli-flags.toml` deliberately excludes
secret-bearing names from command-line flags because process arguments are
observable.

## Build and runtime rules

- Never decrypt during `docker build`; layers and build metadata persist.
- Inject secrets when the process starts, not through build arguments.
- Never print values in CI, logs, shell tracing, GitHub, Linear, or evidence.
- Keep the Rust application as PID 1 so SIGTERM reaches it directly.
- Reject malformed dotenv, duplicate names, unsafe filesystem shapes, and
  unexpected files below `env/enc/`.
- Give humans and workloads separate identities; maintain an independently
  controlled recovery path.
- Removing a SOPS recipient prevents future access only after updatekeys and
  rotation of the underlying application credentials by their owners.

SOPS dotenv values are single-line. Represent multiline values with escaped
newlines. Documentation and test source must not contain a complete private-key
signature; tests that need one construct it at runtime so scanners stay
fail-closed.

## Source-control policy

Plaintext dotenv is denied repository-wide. `env/dec/` is ignored. `env/enc/*`
is denied and then exactly `dev.env.enc` and `prod.env.enc` are re-allowed.
`.sops.yaml` has a separate exact creation rule for each approved ciphertext
path, and `.gitattributes` normalizes ciphertext to LF.

A credential or private identity that appears in source, logs, an issue, a pull
request, chat, or an artifact is exposed. Record only its provider/path/name and
assign rotation to its owner; do not copy the value to another system and do not
revoke or rotate shared credentials without explicit authorization.
