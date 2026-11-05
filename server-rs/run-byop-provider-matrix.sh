#!/usr/bin/env bash
set -euo pipefail

# Opt-in differential gate for managed PostgreSQL providers. Each supplied URL
# is used only by the disposable-schema contract in `live_byop`; no URL is
# printed, persisted, or passed as a command-line argument.
server_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$server_root"
provider_run_id=${INSTANT_BYOP_PROVIDER_RUN_ID:-}

if [[ ${1:-} == --cleanup-only && -z $provider_run_id ]]; then
  echo 'INSTANT_BYOP_PROVIDER_RUN_ID is required for cleanup' >&2
  exit 1
fi
provider_run_id=${provider_run_id:-m${BASHPID}}
if [[ ! $provider_run_id =~ ^[A-Za-z0-9]{1,12}$ ]]; then
  echo 'INSTANT_BYOP_PROVIDER_RUN_ID must be 1-12 ASCII letters or digits' >&2
  exit 1
fi

cleanup_provider() {
  local provider=$1 variable=$2 database_url=${!2:-}
  [[ -n $database_url ]] || return 0
  if [[ ${INSTANT_BYOP_PROVIDER_GATE_CONFIRM:-} != dedicated-provider-test-databases ]]; then
    echo 'INSTANT_BYOP_PROVIDER_GATE_CONFIRM must be dedicated-provider-test-databases' >&2
    return 1
  fi
  echo "Cleaning BYOP test schemas from $provider..."
  INSTANT_BYOP_PROVIDER_CLEANUP_ONLY=1 \
  INSTANT_BYOP_PROVIDER_GATE_CONFIRM="$INSTANT_BYOP_PROVIDER_GATE_CONFIRM" \
  INSTANT_BYOP_PROVIDER_RUN_ID="$provider_run_id" \
  TEST_DATABASE_URL="$database_url" \
    cargo test -p instant-pg --test live_byop cleanup_managed_provider_schemas \
      -- --exact --nocapture
}

run_provider() {
  local provider=$1 variable=$2 database_url=${!2:-}
  if [[ -z $database_url ]]; then
    echo "BYOP $provider provider gate skipped: $variable is not configured"
    return
  fi
  if [[ ${INSTANT_BYOP_PROVIDER_GATE_CONFIRM:-} != dedicated-provider-test-databases ]]; then
    echo 'INSTANT_BYOP_PROVIDER_GATE_CONFIRM must be dedicated-provider-test-databases' >&2
    return 1
  fi
  echo "Running BYOP PostgreSQL schema matrix against $provider..."
  cleanup_provider "$provider" "$variable"
  set +e
  RUST_TEST_THREADS=1 INSTANT_BYOP_PROVIDER_RUN_ID="$provider_run_id" \
  TEST_DATABASE_URL="$database_url" \
    cargo test -p instant-pg --test live_byop -- --nocapture
  local test_status=$?
  set -e
  cleanup_provider "$provider" "$variable"
  return "$test_status"
}

case ${1:-} in
  "")
    run_provider Neon INSTANT_BYOP_NEON_DATABASE_URL
    run_provider Supabase INSTANT_BYOP_SUPABASE_DATABASE_URL
    ;;
  --cleanup-only)
    cleanup_provider Neon INSTANT_BYOP_NEON_DATABASE_URL
    cleanup_provider Supabase INSTANT_BYOP_SUPABASE_DATABASE_URL
    ;;
  *)
    echo 'usage: run-byop-provider-matrix.sh [--cleanup-only]' >&2
    exit 2
    ;;
esac
