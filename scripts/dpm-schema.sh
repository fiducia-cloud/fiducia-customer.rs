#!/usr/bin/env bash
# Converge the canonical customer-plane schema onto a Supabase Postgres target.
# This wrapper keeps credentials in environment variables and makes target
# mutation an explicit second decision after diff/verify have passed.
#
# dpm's CLI flags (--source/--target/--shadow/--schemas) are the flags-2-env
# front end; the installed binary itself reads the ENVIRONMENT VARIABLES those
# flags map to (SOURCE_SQL_FILE, TARGET_DATABASE_URL, SHADOW_DATABASE_URL,
# DPM_SCHEMAS, DPM_YES). We set those directly so this works against a plain
# `dpm` install without the flags-2-env shim.
set -euo pipefail

command=${1:-verify}
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
schema="$root/../fiducia-interfaces/sql/customer.sql"
dpm_bin=${DPM_BIN:-dpm}

usage() {
  printf '%s\n' "usage: scripts/dpm-schema.sh {diff|verify|apply}" >&2
  printf '%s\n' "requires DATABASE_URL and SHADOW_DATABASE_URL (direct Postgres connections)." >&2
}

case "$command" in
  diff|verify|apply) ;;
  *) usage; exit 64 ;;
esac

if ! command -v "$dpm_bin" >/dev/null 2>&1; then
  printf '%s\n' "dpm is required; install with: cargo install --git https://github.com/declarative-migrations/declarative-postgres-migrate.rs --locked" >&2
  exit 127
fi

: "${DATABASE_URL:?DATABASE_URL must point to the customer Supabase database}"
: "${SHADOW_DATABASE_URL:?SHADOW_DATABASE_URL must point to an isolated scratch-capable Postgres database}"

if [[ ! -f "$schema" ]]; then
  printf '%s\n' "canonical customer schema is missing: $schema" >&2
  exit 66
fi

# The declarative schema file is the desired state; the live Supabase database is
# the current state; the shadow server is where dpm materializes the .sql source
# and rehearses. Scope to `public` so managed Supabase schemas (auth, storage, …)
# are never diffed. Set TARGET_DATABASE_URL explicitly rather than relying on the
# DATABASE_URL fallback so the target is unambiguous.
export SOURCE_SQL_FILE="$schema"
export TARGET_DATABASE_URL="$DATABASE_URL"
export SHADOW_DATABASE_URL="$SHADOW_DATABASE_URL"
export DPM_SCHEMAS="${DPM_SCHEMAS:-public}"

case "$command" in
  diff)
    exec "$dpm_bin" diff
    ;;
  verify)
    # `verify` never writes the real target: it rehearses the generated plan on
    # a shadow replica and proves the post-apply catalog converges.
    exec "$dpm_bin" verify
    ;;
  apply)
    # dpm refuses destructive operations unless separate destructive consent is
    # supplied; this wrapper never supplies it. Non-destructive convergence
    # (new tables/columns/indexes) still requires an explicit human opt-in here.
    if [[ ${DPM_APPLY_APPROVED:-} != 1 ]]; then
      printf '%s\n' "refusing apply: run diff and verify first, then set DPM_APPLY_APPROVED=1" >&2
      exit 77
    fi
    export DPM_YES=1
    exec "$dpm_bin" apply
    ;;
esac
