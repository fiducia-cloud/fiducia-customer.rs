# fiducia-customer.rs — task runner. Run `just` to see everything.
#
# Deploy-specific secret values live only in env/enc/{dev,prod}.env.enc or the
# runtime platform secret manager. See env/README.md.

# Safe path metadata only; never execute secret tooling while Just parses.
export FIDUCIA_ENV_DEC := justfile_directory() / "env" / "dec"

import '.just/env.just'

# Show available recipes.
default:
    @just --list

# Canonical keyless encrypted-environment policy audit.
[group('env')]
env-verify:
    @ores-sops verify
