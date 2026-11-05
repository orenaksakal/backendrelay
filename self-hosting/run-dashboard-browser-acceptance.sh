#!/usr/bin/env bash
set -Eeuo pipefail

# D9.2 live dashboard journey: build the dashboard, serve it with `vite preview`
# against a disposable Docker test stack (PostgreSQL + RustFS + Rust server), and
# run the `live` Playwright project. `make verify-dashboard-browser` keeps this
# script as its entry point.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compose_file="$repo_root/self-hosting/docker-compose.test.yml"
config_file="$repo_root/client/dashboard/dist/client/config.json"
server_log=""
preview_log=""
logs_pid=""
preview_pid=""
compose=()
COMPOSE_PROJECT_NAME=""

required_commands=(docker curl node python3)
for command in "${required_commands[@]}"; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done

if command -v mise >/dev/null 2>&1; then
  pnpm_cmd=(mise exec node@24 pnpm@10.34.9 -- pnpm)
else
  command -v pnpm >/dev/null 2>&1 || {
    echo "required command is not installed: pnpm" >&2
    exit 1
  }
  pnpm_cmd=(pnpm)
fi

run_bounded() {
  local label=$1 duration=$2
  shift 2
  echo "==> $label (limit: $duration)"
  if command -v timeout >/dev/null 2>&1; then
    timeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  else
    "$@"
  fi
}

run_from_client() {
  local label=$1 duration=$2
  shift 2
  (
    cd "$repo_root/client"
    run_bounded "$label" "$duration" "$@"
  )
}

evidence_dir=${INSTANT_DASHBOARD_EVIDENCE_DIR:-}
if [[ -z $evidence_dir ]]; then
  evidence_dir="/tmp/backendrelay-dashboard-$(date +%s)-$$"
fi
mkdir -p "$evidence_dir"
server_log="$evidence_dir/server.log"
preview_log="$evidence_dir/preview.log"
: >"$server_log"
: >"$preview_log"

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n $logs_pid ]]; then
    kill "$logs_pid" >/dev/null 2>&1 || true
  fi
  if [[ -n $preview_pid ]]; then
    if command -v pkill >/dev/null 2>&1; then
      pkill -TERM -P "$preview_pid" >/dev/null 2>&1 || true
    fi
    kill "$preview_pid" >/dev/null 2>&1 || true
  fi
  if ((${#compose[@]} > 0)); then
    if ((status != 0)); then
      "${compose[@]}" logs --tail 200 server >&2 || true
      if [[ -s $preview_log ]]; then
        echo '--- vite preview ---' >&2
        tail -n 100 "$preview_log" >&2
      fi
    fi
    if [[ ${INSTANT_DASHBOARD_ACCEPTANCE_KEEP_STACK:-0} != 1 ]]; then
      "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
    else
      echo "Dashboard acceptance stack preserved: COMPOSE_PROJECT_NAME=$COMPOSE_PROJECT_NAME" >&2
    fi
  fi
  echo "Dashboard acceptance evidence: $evidence_dir" >&2
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

prepare_browser_harness() {
  if [[ ${INSTANT_DASHBOARD_ACCEPTANCE_SKIP_INSTALL:-0} != 1 ]]; then
    run_from_client "install dashboard dependencies" \
      "${INSTANT_DASHBOARD_ACCEPTANCE_INSTALL_TIMEOUT:-10m}" \
      "${pnpm_cmd[@]}" install --frozen-lockfile --prefer-offline
  fi
  if [[ -z ${INSTANT_PLAYWRIGHT_WS_ENDPOINT:-} && ${INSTANT_DASHBOARD_ACCEPTANCE_SKIP_PLAYWRIGHT_INSTALL:-0} != 1 ]]; then
    run_from_client "install the Playwright Chromium engine" \
      "${INSTANT_DASHBOARD_ACCEPTANCE_PLAYWRIGHT_TIMEOUT:-10m}" \
      "${pnpm_cmd[@]}" --filter backendrelay-dashboard exec playwright install chromium
  fi
}

# Keep disposable acceptance away from an operator's active local development
# services. Bind all reservations at once so this selection cannot return the
# same port for two services.
read -r server_port dashboard_port postgres_port s3_port < <(python3 - <<'PY'
import socket
sockets = [socket.socket() for _ in range(4)]
try:
    for listener in sockets:
        listener.bind(('127.0.0.1', 0))
    print(*(listener.getsockname()[1] for listener in sockets))
finally:
    for listener in sockets:
        listener.close()
PY
)
export INSTANT_SERVER_PORT=${INSTANT_SERVER_PORT:-$server_port}
export INSTANT_DASHBOARD_PORT=${INSTANT_DASHBOARD_PORT:-$dashboard_port}
export INSTANT_POSTGRES_PORT=${INSTANT_POSTGRES_PORT:-$postgres_port}
export INSTANT_S3_TEST_PORT=${INSTANT_S3_TEST_PORT:-$s3_port}
export COMPOSE_PROJECT_NAME="instant-dashboard-live-$$"
compose=(docker compose --project-name "$COMPOSE_PROJECT_NAME" --file "$compose_file")

export S3_PUBLIC_ENDPOINT=http://127.0.0.1:${INSTANT_S3_TEST_PORT}
export INSTANT_BACKEND_URL=http://127.0.0.1:${INSTANT_SERVER_PORT}
export INSTANT_DASHBOARD_URL=http://127.0.0.1:${INSTANT_DASHBOARD_PORT}
export INSTANT_TEST_CORS_ORIGIN=$INSTANT_DASHBOARD_URL

"${compose[@]}" down --volumes --remove-orphans
prepare_browser_harness

run_from_client "build dashboard workspace packages" \
  "${INSTANT_DASHBOARD_ACCEPTANCE_PACKAGES_TIMEOUT:-15m}" \
  "${pnpm_cmd[@]}" build-packages
run_from_client "build the dashboard for the live server" \
  "${INSTANT_DASHBOARD_ACCEPTANCE_BUILD_TIMEOUT:-15m}" \
  env -u VITE_MOCK_API -u VITE_CATALOG \
  "${pnpm_cmd[@]}" --filter backendrelay-dashboard build

# `vite preview` serves the static build, so write the container's runtime
# config into that static root after building. The keys and the empty optional
# values mirror `client/dashboard/deploy/Caddyfile`; the preview readiness loop
# below fails if the file is not served.
python3 - "$config_file" "$INSTANT_BACKEND_URL" "$S3_PUBLIC_ENDPOINT" <<'PY'
import json
import sys

path, api_url, storage_origin = sys.argv[1:4]
config = {
    "apiBaseUrl": api_url,
    "dashboardApiUrl": "",
    "storageOrigin": storage_origin,
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(config, handle, indent=2)
    handle.write("\n")
PY

run_bounded "build the Rust server image" \
  "${INSTANT_DASHBOARD_ACCEPTANCE_BUILD_TIMEOUT:-30m}" \
  "${compose[@]}" build server

if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  run_bounded "pull pinned PostgreSQL image" \
    "${INSTANT_DASHBOARD_ACCEPTANCE_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull postgres
else
  run_bounded "build local PostgreSQL image" \
    "${INSTANT_DASHBOARD_ACCEPTANCE_BUILD_TIMEOUT:-30m}" \
    "${compose[@]}" build postgres
fi

"${compose[@]}" up -d --no-build postgres s3 createbuckets
postgres_ready_checks=0
for _ in $(seq 1 60); do
  if "${compose[@]}" exec -T postgres pg_isready --username postgres --dbname instant >/dev/null 2>&1; then
    ((postgres_ready_checks += 1))
    if ((postgres_ready_checks >= 2)); then
      break
    fi
  else
    postgres_ready_checks=0
  fi
  sleep 1
done
if ((postgres_ready_checks < 2)); then
  echo 'PostgreSQL did not become ready within 60 seconds' >&2
  exit 1
fi

run_bounded "migrate the live acceptance database" \
  "${INSTANT_DASHBOARD_ACCEPTANCE_MIGRATE_TIMEOUT:-5m}" \
  "${compose[@]}" run --rm --no-deps server migrate up
"${compose[@]}" up -d --no-build server

server_ready=false
for _ in $(seq 1 90); do
  if curl --silent --show-error --fail --max-time 2 \
      "$INSTANT_BACKEND_URL/health/system" >/dev/null 2>&1; then
    server_ready=true
    break
  fi
  sleep 2
done
if [[ $server_ready != true ]]; then
  echo 'Rust server did not become ready within 180 seconds' >&2
  exit 1
fi

# The test stack has no email provider, so the server logs each dashboard magic
# code. The live spec reads the newest code for its email from this file.
"${compose[@]}" logs --no-color --follow server >"$server_log" 2>&1 &
logs_pid=$!

(
  cd "$repo_root/client/dashboard"
  exec "${pnpm_cmd[@]}" exec vite preview --host 127.0.0.1 \
    --port "$INSTANT_DASHBOARD_PORT" --strictPort
) >"$preview_log" 2>&1 &
preview_pid=$!

dashboard_ready=false
for _ in $(seq 1 60); do
  config_json=$(curl --silent --show-error --fail --max-time 2 \
    "$INSTANT_DASHBOARD_URL/config.json" 2>/dev/null) || config_json=""
  if [[ $config_json == *"\"apiBaseUrl\": \"$INSTANT_BACKEND_URL\""* ]]; then
    dashboard_ready=true
    break
  fi
  sleep 2
done
if [[ $dashboard_ready != true ]]; then
  echo 'Dashboard preview did not serve the live /config.json within 120 seconds' >&2
  exit 1
fi

export BR_LIVE=1
export BR_LIVE_DASHBOARD_URL=$INSTANT_DASHBOARD_URL
export BR_LIVE_SERVER_LOG=$server_log
(
  cd "$repo_root/client/dashboard"
  run_bounded "live dashboard journey" \
    "${INSTANT_DASHBOARD_ACCEPTANCE_TEST_TIMEOUT:-20m}" \
    "${pnpm_cmd[@]}" exec playwright test --project live
)
