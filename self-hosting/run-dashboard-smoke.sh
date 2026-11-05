#!/usr/bin/env bash
set -Eeuo pipefail

# Cheap, blocking complement to the nightly browser matrix: prove that the built
# dashboard actually serves pages. A TanStack version skew once made every /dash
# request answer HTTP 500 with `serverSsr.reserveStreamFastPath is not a
# function` while tsc, vitest and vite build stayed green. This harness builds
# the real images, starts the disposable stack, and checks the rendered HTML
# with curl only.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compose_file="$repo_root/self-hosting/docker-compose.test.yml"

for command in docker curl python3; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done

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

if [[ -n ${INSTANT_SERVER_IMAGE:-} || -n ${INSTANT_DASHBOARD_IMAGE:-} ]]; then
  : "${INSTANT_SERVER_IMAGE:?candidate runs require both exact image references}"
  : "${INSTANT_DASHBOARD_IMAGE:?candidate runs require both exact image references}"
  "$repo_root/self-hosting/verify-release-images.sh" --shape-only \
    "$INSTANT_SERVER_IMAGE" "$INSTANT_DASHBOARD_IMAGE"
  candidate_images=true
else
  candidate_images=false
fi

# Keep disposables away from an operator's active local development services.
# Bind all reservations at once so this selection cannot return the same port
# for two services.
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
export COMPOSE_PROJECT_NAME="instant-dashboard-smoke-$$"
compose=(docker compose --project-name "$COMPOSE_PROJECT_NAME" --file "$compose_file")
dashboard_origin="http://127.0.0.1:$INSTANT_DASHBOARD_PORT"
cleanup() {
  status=$?
  trap - EXIT INT TERM
  if ((status != 0)); then
    "${compose[@]}" logs --tail 200 postgres server www >&2 || true
  fi
  if [[ ${INSTANT_DASHBOARD_SMOKE_KEEP_STACK:-0} != 1 ]]; then
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  else
    echo "Dashboard smoke stack preserved: COMPOSE_PROJECT_NAME=$COMPOSE_PROJECT_NAME" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

"${compose[@]}" down --volumes --remove-orphans

if [[ $candidate_images == true ]]; then
  run_bounded "pull paired dashboard and server candidates" \
    "${INSTANT_DASHBOARD_SMOKE_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull server www
else
  run_bounded "build local dashboard and Rust server images" \
    "${INSTANT_DASHBOARD_SMOKE_BUILD_TIMEOUT:-30m}" \
    "${compose[@]}" build server www
fi

if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  run_bounded "pull pinned PostgreSQL image" \
    "${INSTANT_DASHBOARD_SMOKE_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull postgres
else
  run_bounded "build local PostgreSQL image" \
    "${INSTANT_DASHBOARD_SMOKE_BUILD_TIMEOUT:-30m}" \
    "${compose[@]}" build postgres
fi

export S3_PUBLIC_ENDPOINT=http://127.0.0.1:${INSTANT_S3_TEST_PORT:-9000}
export INSTANT_BACKEND_URL=http://localhost:${INSTANT_SERVER_PORT:-8888}
export INSTANT_DASHBOARD_URL=http://localhost:${INSTANT_DASHBOARD_PORT:-3000}

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

run_bounded "migrate dashboard smoke database" \
  "${INSTANT_DASHBOARD_SMOKE_MIGRATE_TIMEOUT:-5m}" \
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

"${compose[@]}" up -d --no-build www
dashboard_ready=false
for _ in $(seq 1 60); do
  if curl --silent --show-error --fail --max-time 2 \
      "$INSTANT_DASHBOARD_URL/publicSelfHostedVariables.js" >/dev/null 2>&1; then
    dashboard_ready=true
    break
  fi
  sleep 2
done
if [[ $dashboard_ready != true ]]; then
  echo 'Dashboard did not become ready within 120 seconds' >&2
  exit 1
fi

# /dash is client-rendered (the route sets ssr: false), so the login form is not
# in the response. Assert the server-rendered document instead: HTML, an
# application-name marker, and references to the compiled dashboard bundle. A
# broken Nitro SSR layer answers with a JSON error payload, not this document.
# The launcher starts the dashboard process before the site process, so retry
# briefly; a version-skewed build keeps answering 500 and still fails below.
dash_response=''
for _ in $(seq 1 30); do
  if dash_response=$(curl --silent --max-time 10 \
      --write-out $'\n%{http_code}' "$dashboard_origin/dash"); then
    if [[ ${dash_response##*$'\n'} == 200 ]]; then
      break
    fi
  fi
  sleep 2
done
dash_html=${dash_response%$'\n'*}
dash_status=${dash_response##*$'\n'}
if [[ $dash_status != 200 ]]; then
  echo "GET /dash returned HTTP $dash_status instead of 200" >&2
  exit 1
fi
if [[ $dash_html == *'"status":500'* || $dash_html == *'reserveStreamFastPath'* ]]; then
  echo 'GET /dash returned a Nitro SSR error payload' >&2
  exit 1
fi
if [[ $dash_html != *'<!DOCTYPE html'* && $dash_html != *'<!doctype html'* ]]; then
  echo 'GET /dash did not return an HTML document' >&2
  exit 1
fi
if [[ $dash_html != *'application-name'* && $dash_html != *'Enter your email address'* ]]; then
  echo 'GET /dash HTML is missing the server-rendered dashboard markers' >&2
  exit 1
fi
if [[ $dash_html != *'/_build/'* && $dash_html != *'Enter your email address'* ]]; then
  echo 'GET /dash HTML does not reference the compiled dashboard bundle' >&2
  exit 1
fi

# The self-hosted site has no marketing page left: / answers with a redirect to
# the dashboard. Accept either that redirect or a direct 200 so restoring the
# marketing site keeps this check honest.
read -r root_status root_redirect < <(curl --silent --show-error --max-time 30 \
  --output /dev/null --write-out '%{http_code} %{redirect_url}\n' "$dashboard_origin/")
if [[ $root_status == 200 ]]; then
  :
elif [[ $root_status =~ ^30[2378]$ && $root_redirect == */dash ]]; then
  :
else
  echo "GET / returned HTTP $root_status instead of 200 or the redirect to /dash" >&2
  exit 1
fi

echo 'Dashboard runtime smoke test passed'
