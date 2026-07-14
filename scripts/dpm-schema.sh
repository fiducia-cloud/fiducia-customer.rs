#!/usr/bin/env bash
# Converge the canonical customer-plane schema onto a Supabase Postgres target.
# This wrapper deliberately keeps credentials in environment variables and makes
# target mutation an explicit second decision after diff/verify have passed.
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

common=(--source "$schema" --target "$DATABASE_URL" --shadow "$SHADOW_DATABASE_URL" --schemas public)

case "$command" in
  diff)
    exec "$dpm_bin" diff "${common[@]}"
    ;;
  verify)
    # `verify` never writes the real target: it rehearses the generated plan on
    # a shadow replica and proves the post-apply catalog converges.
    exec "$dpm_bin" verify "${common[@]}"
    ;;
  apply)
    # DPM itself refuses destructive operations unless separate destructive
    # consent is supplied. This wrapper never supplies that consent.
    if [[ ${DPM_APPLY_APPROVED:-} != 1 ]]; then
      printf '%s\n' "refusing apply: run diff and verify first, then set DPM_APPLY_APPROVED=1" >&2
      exit 77
    fi
    exec "$dpm_bin" apply "${common[@]}" --yes
    ;;
esac
