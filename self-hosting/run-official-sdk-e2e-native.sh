#!/usr/bin/env bash
set -Eeuo pipefail

# Runs the endpoint-configurable official SDK suites against a native Rust
# process, a disposable local PostgreSQL cluster, and real Cloudflare R2.
# Docker and an S3 emulator are deliberately not used.

readonly POSTGRES_VERSION=17.11
readonly WAL2JSON_TAG=wal2json_2_6
readonly NODE_VERSION=24
readonly PNPM_VERSION=10.34.9

repo_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
server_root="$repo_root/server-rs"
client_root="$repo_root/client"
env_file="${INSTANT_NATIVE_E2E_ENV_FILE:-$repo_root/self-hosting/.env}"
# rustup selects rust-toolchain.toml from the working directory, not --manifest-path.
cd "$server_root"
suite=smoke
preflight_only=false

usage() {
  cat <<'EOF'
usage: run-official-sdk-e2e-native.sh [--suite smoke|core|standard|full] [--preflight]

  smoke     Build the clients and run the smallest official Core browser test.
  core      Run smoke, then the complete official Core browser suite.
  standard  Run smoke, complete Core, Admin, and Platform suites.
  full      Run standard, then every remaining general official SDK wrapper.

  --preflight  Validate local commands, R2 configuration, package names,
               package scripts, and fixture paths without installing or running.

R2_ACCOUNT_ID, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY, R2_FILES_BUCKET, and
R2_BACKUPS_BUCKET may be exported explicitly or loaded from self-hosting/.env.
Explicit non-empty environment values take precedence. Secrets are never logged.

Timeout overrides: INSTANT_NATIVE_E2E_SETUP_TIMEOUT,
INSTANT_NATIVE_E2E_MIGRATION_TIMEOUT, INSTANT_NATIVE_E2E_READINESS_SECONDS,
INSTANT_NATIVE_E2E_INSTALL_TIMEOUT, INSTANT_NATIVE_E2E_BUILD_TIMEOUT,
INSTANT_NATIVE_E2E_SMOKE_TIMEOUT, and INSTANT_NATIVE_E2E_SUITE_TIMEOUT.
EOF
}

while (($#)); do
  case "$1" in
    --suite)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      suite=$2
      shift 2
      ;;
    --preflight)
      preflight_only=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

case "$suite" in
  smoke|core|standard|full) ;;
  *) usage >&2; exit 2 ;;
esac

for command in mise git make python3 cargo curl env stat; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done

validate_client_contract() {
  python3 - "$client_root" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
contracts = {
    ".": ("backendrelay-monorepo", "build-packages"),
    "packages/cli": ("instant-cli", "test:e2e"),
    "packages/core": ("@instantdb/core", "test:e2e"),
    "packages/resumable-stream": ("@instantdb/resumable-stream", "test:ci"),
    "packages/admin": ("@instantdb/admin", "test:e2e"),
    "packages/platform": ("@instantdb/platform", "test:e2e"),
    "packages/react": ("@instantdb/react", "test:e2e"),
    "packages/react-native": ("@instantdb/react-native", "test:e2e"),
    "packages/webhooks": ("@instantdb/webhooks", "test:e2e"),
}
errors = []
for relative, (expected_name, script) in contracts.items():
    manifest = root / relative / "package.json"
    if not manifest.is_file():
        errors.append(f"missing {manifest}")
        continue
    package = json.loads(manifest.read_text())
    if package.get("name") != expected_name:
        errors.append(
            f"{manifest}: expected package name {expected_name!r}, got {package.get('name')!r}"
        )
    if script not in package.get("scripts", {}):
        errors.append(f"{manifest}: missing script {script!r}")

fixtures = [
    "packages/core/__tests__/src/simple.e2e.test.ts",
]
for relative in fixtures:
    if not (root / relative).is_file():
        errors.append(f"missing {root / relative}")

if errors:
    raise SystemExit("\n".join(errors))
PY
}

validate_client_contract

# Parse only the five R2 assignments used by this harness. This intentionally
# does not source or execute the ignored operator file.
load_r2_env() {
  [[ -f "$env_file" ]] || return 0
  local key value
  while IFS='=' read -r key value; do
    case "$key" in
      R2_ACCOUNT_ID|R2_ACCESS_KEY_ID|R2_SECRET_ACCESS_KEY|R2_FILES_BUCKET|R2_BACKUPS_BUCKET)
        [[ -n "${!key:-}" ]] && continue
        value=${value%$'\r'}
        if [[ "$value" == \"*\" && "$value" == *\" ]]; then
          value=${value:1:${#value}-2}
        elif [[ "$value" == \'*\' && "$value" == *\' ]]; then
          value=${value:1:${#value}-2}
        fi
        printf -v "$key" '%s' "$value"
        ;;
    esac
  done <"$env_file"
}

if [[ -f "$env_file" ]]; then
  env_mode=$(stat -c '%a' "$env_file" 2>/dev/null || stat -f '%Lp' "$env_file" 2>/dev/null || true)
  if [[ ${env_mode: -2} != 00 ]]; then
    echo "$env_file contains deployment secrets and must not be group/world accessible" >&2
    exit 1
  fi
fi

load_r2_env
for name in R2_ACCOUNT_ID R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY R2_FILES_BUCKET R2_BACKUPS_BUCKET; do
  [[ -n "${!name:-}" ]] || {
    echo "$name must be set explicitly or in $env_file" >&2
    exit 1
  }
done

if [[ "$preflight_only" == true ]]; then
  echo "Native official SDK preflight passed for stage '$suite'"
  exit 0
fi

readonly setup_timeout=${INSTANT_NATIVE_E2E_SETUP_TIMEOUT:-10m}
readonly migration_timeout=${INSTANT_NATIVE_E2E_MIGRATION_TIMEOUT:-10m}
readonly readiness_seconds=${INSTANT_NATIVE_E2E_READINESS_SECONDS:-600}
readonly install_timeout=${INSTANT_NATIVE_E2E_INSTALL_TIMEOUT:-10m}
readonly build_timeout=${INSTANT_NATIVE_E2E_BUILD_TIMEOUT:-10m}
readonly smoke_timeout=${INSTANT_NATIVE_E2E_SMOKE_TIMEOUT:-5m}
readonly suite_timeout=${INSTANT_NATIVE_E2E_SUITE_TIMEOUT:-10m}

run_bounded() {
  local label=$1 duration=$2
  shift 2
  echo "==> $label (limit: $duration)"
  if command -v timeout >/dev/null 2>&1; then
    timeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  else
    python3 -c '
import os
import re
import signal
import subprocess
import sys

duration = sys.argv[1]
match = re.fullmatch(r"([0-9]+(?:\.[0-9]*)?|\.[0-9]+)([smhd]?)", duration)
if match is None:
    print(f"invalid timeout duration: {duration}", file=sys.stderr)
    raise SystemExit(125)

seconds = float(match.group(1)) * {
    "": 1,
    "s": 1,
    "m": 60,
    "h": 60 * 60,
    "d": 24 * 60 * 60,
}[match.group(2)]
if seconds <= 0:
    print(f"timeout duration must be positive: {duration}", file=sys.stderr)
    raise SystemExit(125)

try:
    process = subprocess.Popen(sys.argv[2:], start_new_session=True)
except FileNotFoundError as error:
    print(error, file=sys.stderr)
    raise SystemExit(127)
except PermissionError as error:
    print(error, file=sys.stderr)
    raise SystemExit(126)

def signal_group(signal_number):
    try:
        os.killpg(process.pid, signal_number)
    except ProcessLookupError:
        pass

def forward_signal(signal_number, _frame):
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal_group(signal_number)
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        signal_group(signal.SIGKILL)
        process.wait()
    raise SystemExit(128 + signal_number)

signal.signal(signal.SIGINT, forward_signal)
signal.signal(signal.SIGTERM, forward_signal)

try:
    status = process.wait(timeout=seconds)
except subprocess.TimeoutExpired:
    signal_group(signal.SIGTERM)
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        signal_group(signal.SIGKILL)
        process.wait()
    raise SystemExit(124)

raise SystemExit(128 - status if status < 0 else status)
' "$duration" "$@"
  fi
}

redact_logs() {
  sed -E \
    -e 's#(postgres(ql)?://[^:/@[:space:]]+):[^@[:space:]]+@#\1:[REDACTED]@#g' \
    -e 's#(Bearer )[A-Za-z0-9._~+/=-]+#\1[REDACTED]#g' \
    -e 's#((R2|S3|AEAD|WEBHOOK)[_A-Z]*(KEY|SECRET|TOKEN)[_A-Z]*=)[^[:space:]]+#\1[REDACTED]#g'
}

run_root=$(mktemp -d "${TMPDIR:-/tmp}/instant-native-sdk-e2e.XXXXXX")
data_root="$run_root/postgres"
postgres_log="$run_root/postgres.log"
server_log="$run_root/server.log"
secrets_file="$run_root/secrets.env"
server_pid=
postgres_bin=

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -n "$postgres_bin" && -f "$data_root/postmaster.pid" ]]; then
    "$postgres_bin/pg_ctl" -D "$data_root" -m immediate -w stop >/dev/null 2>&1 || true
  fi
  if ((status != 0)); then
    if [[ -s "$server_log" ]]; then
      echo "Last native server log lines (credentials redacted):" >&2
      tail -n 200 "$server_log" | redact_logs >&2
    fi
    if [[ -s "$postgres_log" ]]; then
      echo "Last PostgreSQL log lines:" >&2
      tail -n 100 "$postgres_log" >&2
    fi
  fi
  case "$run_root" in
    */instant-native-sdk-e2e.*) rm -rf -- "$run_root" ;;
    *) echo "refusing to remove unexpected temporary path: $run_root" >&2 ;;
  esac
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_bounded "install PostgreSQL $POSTGRES_VERSION" "$setup_timeout" \
  mise install "postgres@$POSTGRES_VERSION"
postgres_root=$(mise where "postgres@$POSTGRES_VERSION")
postgres_bin="$postgres_root/bin"

wal2json_library="$postgres_root/lib/wal2json.so"
if [[ ! -f "$wal2json_library" && ! -f "$postgres_root/lib/wal2json.dylib" ]]; then
  wal2json_cache="$postgres_root/instant-acceptance-cache/wal2json-$WAL2JSON_TAG"
  if [[ ! -d "$wal2json_cache/.git" ]]; then
    run_bounded "download wal2json $WAL2JSON_TAG" "$setup_timeout" \
      git clone --depth 1 --branch "$WAL2JSON_TAG" \
        https://github.com/eulerto/wal2json.git "$wal2json_cache"
  fi
  run_bounded "build wal2json $WAL2JSON_TAG" "$setup_timeout" \
    make -C "$wal2json_cache" PG_CONFIG="$postgres_bin/pg_config"
  run_bounded "install wal2json $WAL2JSON_TAG" "$setup_timeout" \
    make -C "$wal2json_cache" PG_CONFIG="$postgres_bin/pg_config" install
fi

postgres_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
server_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
while [[ "$server_port" == "$postgres_port" ]]; do
  server_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
done

run_bounded "initialize disposable PostgreSQL" "$setup_timeout" \
  "$postgres_bin/initdb" --pgdata="$data_root" --username=instant_rust \
    --auth=trust --encoding=UTF8 --no-locale >/dev/null
printf '%s\n' \
  "listen_addresses = '127.0.0.1'" \
  "port = $postgres_port" \
  "unix_socket_directories = '$run_root'" \
  "wal_level = logical" \
  "max_replication_slots = 10" \
  "max_wal_senders = 10" \
  "output_plugin_libraries = 'wal2json'" \
  "timezone = 'UTC'" \
  "fsync = off" \
  "synchronous_commit = off" \
  "full_page_writes = off" \
  >>"$data_root/postgresql.conf"
run_bounded "start disposable PostgreSQL" "$setup_timeout" \
  "$postgres_bin/pg_ctl" -D "$data_root" -l "$postgres_log" -w start >/dev/null
run_bounded "create disposable database" "$setup_timeout" \
  "$postgres_bin/createdb" -h 127.0.0.1 -p "$postgres_port" -U instant_rust instant

database_url="postgresql://instant_rust@127.0.0.1:$postgres_port/instant?sslmode=disable"
server_origin="http://127.0.0.1:$server_port"
r2_endpoint="https://$R2_ACCOUNT_ID.r2.cloudflarestorage.com"

run_bounded "generate ephemeral server secrets" "$migration_timeout" \
  bash -c 'umask 077; exec cargo run -q --manifest-path "$1" -p instant-server --no-default-features -- generate-secrets >"$2"' \
    _ "$server_root/Cargo.toml" "$secrets_file"
# Generated content is constrained to shell-safe assignments by instant-server.
set -a
# shellcheck disable=SC1090
. "$secrets_file"
set +a

run_bounded "migrate disposable PostgreSQL" "$migration_timeout" \
  env DATABASE_URL="$database_url" cargo run -q --manifest-path "$server_root/Cargo.toml" \
    -p instant-server --no-default-features -- migrate up

# Export server configuration separately so secret values are inherited through
# the child environment and never appear in the process command line.
export DATABASE_URL="$database_url"
export PORT="$server_port"
export INSTANT_BACKEND_URL="$server_origin"
export INSTANT_DASHBOARD_URL=http://localhost:3000
# Only this disposable server accepts the ephemeral apps used by SDK fixtures.
export INSTANT_EPHEMERAL_APPS=true
export WAL_HISTORY_STORAGE=pg
export CONNECTION_POOL_SIZE=20
export INSTANT_S3_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export INSTANT_S3_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"
export INSTANT_S3_REGION=auto
export S3_ENDPOINT="$r2_endpoint"
export S3_PUBLIC_ENDPOINT="$r2_endpoint"
export S3_BUCKET="$R2_FILES_BUCKET"
export S3_APP_BACKUPS_BUCKET="$R2_BACKUPS_BUCKET"
# Explicit operator values may themselves have arrived exported. Keep only the
# mapped server contract in the child environment, then remove that immediately
# after launch so no SDK/test subprocess inherits storage credentials.
export -n R2_ACCOUNT_ID R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
export -n R2_FILES_BUCKET R2_BACKUPS_BUCKET
cargo run -q --manifest-path "$server_root/Cargo.toml" -p instant-server --no-default-features -- serve \
  >"$server_log" 2>&1 &
server_pid=$!
export -n DATABASE_URL PORT INSTANT_BACKEND_URL INSTANT_DASHBOARD_URL
export -n INSTANT_EPHEMERAL_APPS
export -n WAL_HISTORY_STORAGE CONNECTION_POOL_SIZE
export -n INSTANT_AEAD_KEYSET_JSON INSTANT_WEBHOOK_KEYSET_JSON INSTANT_RATE_LIMIT_HMAC_KEY
export -n INSTANT_S3_ACCESS_KEY_ID INSTANT_S3_SECRET_ACCESS_KEY INSTANT_S3_REGION
export -n S3_ENDPOINT S3_PUBLIC_ENDPOINT S3_BUCKET S3_APP_BACKUPS_BUCKET

echo "==> wait for native Rust server (limit: ${readiness_seconds}s)"
ready=false
for ((attempt = 0; attempt < readiness_seconds; attempt += 2)); do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "native Rust server exited before readiness" >&2
    exit 1
  fi
  if curl --silent --show-error --fail --max-time 2 \
      -X POST "$server_origin/dash/apps/ephemeral" \
      -H 'content-type: application/json' \
      --data '{"title":"native-sdk-e2e-readiness"}' >/dev/null 2>&1; then
    ready=true
    break
  fi
  sleep 2
done
[[ "$ready" == true ]] || {
  echo "native Rust server did not become ready within ${readiness_seconds}s" >&2
  exit 1
}

run_bounded "install pinned Node $NODE_VERSION" "$install_timeout" \
  mise install "node@$NODE_VERSION"
pnpm=(mise exec "node@$NODE_VERSION" "pnpm@$PNPM_VERSION" -- pnpm)
run_bounded "install official client dependencies" "$install_timeout" \
  "${pnpm[@]}" --dir "$client_root" install --frozen-lockfile --prefer-offline
if [[ -n "${PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH:-}" ]]; then
  [[ -x "$PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH" ]] || {
    echo "PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH must name an executable browser" >&2
    exit 1
  }
  echo "==> reuse configured Playwright Chromium"
else
  run_bounded "install pinned Playwright Chromium" "$install_timeout" \
    "${pnpm[@]}" --dir "$client_root" --filter @instantdb/core exec playwright install chromium
fi
run_bounded "build official client packages" "$build_timeout" \
  "${pnpm[@]}" --dir "$client_root" run build-packages

# A caller may have BYOP acceptance variables exported from a previous run.
# General SDK stages must keep the BYOP-only fixtures disabled.
general_env=(env \
  -u INSTANT_BYOP_LIVE_E2E \
  -u INSTANT_BYOP_APP_ID \
  -u INSTANT_BYOP_ADMIN_TOKEN \
  -u INSTANT_BYOP_COMPOSE_FILE \
  -u INSTANT_BYOP_MUTATOR_URL \
  -u INSTANT_BYOP_MUTATOR_TOKEN \
  -u INSTANT_BYOP_PSQL_BIN \
  -u INSTANT_BYOP_PSQL_URL \
  -u INSTANT_BYOP_PSQL_USER \
  -u INSTANT_BYOP_PSQL_DATABASE \
  -u INSTANT_BYOP_SCHEMA)

run_bounded "official Core browser smoke" "$smoke_timeout" \
  "${general_env[@]}" INSTANT_E2E_API_URL="$server_origin" \
    "${pnpm[@]}" --dir "$client_root" --filter @instantdb/core exec vitest run \
      --project e2e __tests__/src/simple.e2e.test.ts

if [[ "$suite" == core || "$suite" == standard || "$suite" == full ]]; then
  run_bounded "remaining official Core browser suite" "$suite_timeout" \
    "${general_env[@]}" INSTANT_E2E_API_URL="$server_origin" \
      "${pnpm[@]}" --dir "$client_root" --filter @instantdb/core exec vitest run \
        --project e2e --exclude '**/simple.e2e.test.ts'
fi

if [[ "$suite" == standard || "$suite" == full ]]; then
  run_bounded "official Admin SDK suite" "$suite_timeout" \
    "${general_env[@]}" INSTANT_ADMIN_LIVE_E2E=1 INSTANT_API_URI="$server_origin" \
      "${pnpm[@]}" --dir "$client_root" --filter @instantdb/admin test:e2e -- --run
  run_bounded "official Platform SDK suite" "$suite_timeout" \
    "${general_env[@]}" INSTANT_API_URI="$server_origin" \
      "${pnpm[@]}" --dir "$client_root" --filter @instantdb/platform test:e2e -- --run
fi

if [[ "$suite" == full ]]; then
  run_bounded "official CLI suite" "$suite_timeout" "${general_env[@]}" \
    INSTANT_CLI_API_URI="$server_origin" INSTANT_CLI_E2E_TIMEOUT_MS=60000 \
    "${pnpm[@]}" --dir "$client_root" --filter instant-cli exec vitest run \
      --project e2e
  run_bounded "official resumable-stream suite" "$suite_timeout" "${general_env[@]}" INSTANT_API_URI="$server_origin" \
    "${pnpm[@]}" --dir "$client_root" --filter @instantdb/resumable-stream test:ci -- --reporter=verbose
  run_bounded "official React SDK suite" "$suite_timeout" \
    "${general_env[@]}" INSTANT_REACT_LIVE_E2E=1 INSTANT_API_URI="$server_origin" \
      "${pnpm[@]}" --dir "$client_root" --filter @instantdb/react test:e2e -- --run
  run_bounded "official React Native SDK suite" "$suite_timeout" \
    "${general_env[@]}" INSTANT_REACT_NATIVE_LIVE_E2E=1 INSTANT_API_URI="$server_origin" \
      "${pnpm[@]}" --dir "$client_root" --filter @instantdb/react-native test:e2e -- --run
  run_bounded "official Webhooks SDK suite" "$suite_timeout" \
    "${general_env[@]}" INSTANT_WEBHOOKS_LIVE_E2E=1 INSTANT_API_URI="$server_origin" \
      "${pnpm[@]}" --dir "$client_root" --filter @instantdb/webhooks test:e2e -- --run
fi

echo "Native official SDK stage '$suite' passed against $server_origin"
