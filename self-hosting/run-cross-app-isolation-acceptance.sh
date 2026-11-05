#!/usr/bin/env bash
set -Eeuo pipefail

# Prove that one app's credentials cannot reach another app over the real
# server: admin HTTP, WebSocket realtime, SSE, presigned storage URLs, rooms,
# streams and webhook configuration. This uses the hermetic test Compose stack
# because presigned URLs, SSE and webhook config need the full server with S3.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compose_file="$repo_root/self-hosting/docker-compose.test.yml"
export COMPOSE_PROJECT_NAME=instant-cross-app-isolation-$$-$RANDOM

available_ports=()
while IFS= read -r acceptance_port; do
  available_ports+=("$acceptance_port")
done < <(python3 - <<'PY'
import socket

listeners = [socket.socket() for _ in range(3)]
try:
    for listener in listeners:
        listener.bind(("127.0.0.1", 0))
    for listener in listeners:
        print(listener.getsockname()[1])
finally:
    for listener in listeners:
        listener.close()
PY
)

server_port=${INSTANT_SERVER_PORT:-${available_ports[0]}}
postgres_port=${INSTANT_POSTGRES_PORT:-${available_ports[1]}}
s3_port=${INSTANT_S3_TEST_PORT:-${available_ports[2]}}
for port in "$server_port" "$postgres_port" "$s3_port"; do
  [[ $port =~ ^[1-9][0-9]{0,4}$ ]] && ((port <= 65535)) || {
    echo 'Acceptance ports must be valid nonzero TCP ports.' >&2
    exit 2
  }
done
[[ $(printf '%s\n' "$server_port" "$postgres_port" "$s3_port" | sort -u | wc -l) -eq 3 ]] || {
  echo 'Acceptance ports must be distinct.' >&2
  exit 2
}

export INSTANT_SERVER_PORT=$server_port
export INSTANT_POSTGRES_PORT=$postgres_port
export INSTANT_S3_TEST_PORT=$s3_port
# The probe fetches presigned URLs from the host, so the public object endpoint
# must be reachable here, not only from inside the Compose network.
export S3_PUBLIC_ENDPOINT=${S3_PUBLIC_ENDPOINT:-http://127.0.0.1:$s3_port}
compose=(docker compose --project-name "$COMPOSE_PROJECT_NAME" --file "$compose_file")
server_origin="http://127.0.0.1:$server_port"
s3_origin=$S3_PUBLIC_ENDPOINT
server_image=${INSTANT_SERVER_IMAGE:-}
owns_project=false

if command -v mise >/dev/null 2>&1; then
  node_cmd=(mise exec node@24 -- node)
else
  node_cmd=(node)
fi

for command in docker curl python3; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done
if [[ ${node_cmd[0]} == node ]]; then
  command -v node >/dev/null 2>&1 || {
    echo 'required command is not installed: node' >&2
    exit 1
  }
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

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if ((status != 0)); then
    "${compose[@]}" logs --tail 200 server postgres >&2 || true
  fi
  if [[ $owns_project == true ]]; then
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ -n $("${compose[@]}" ps -aq) ]] ||
    [[ -n $(docker volume ls --quiet --filter "label=com.docker.compose.project=$COMPOSE_PROJECT_NAME") ]]; then
  echo 'Refusing to use a Compose project that already owns containers or volumes.' >&2
  exit 1
fi
owns_project=true

if [[ -n $server_image ]]; then
  "$repo_root/self-hosting/verify-release-images.sh" --shape-only "$server_image"
  run_bounded "pull candidate Rust server image" \
    "${INSTANT_CROSS_APP_PULL_TIMEOUT:-10m}" "${compose[@]}" pull server
else
  run_bounded "build local Rust server image" \
    "${INSTANT_CROSS_APP_BUILD_TIMEOUT:-20m}" "${compose[@]}" build server
fi

if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  run_bounded "pull pinned PostgreSQL image" \
    "${INSTANT_CROSS_APP_PULL_TIMEOUT:-10m}" "${compose[@]}" pull postgres
else
  run_bounded "build local PostgreSQL image" \
    "${INSTANT_CROSS_APP_BUILD_TIMEOUT:-20m}" "${compose[@]}" build postgres
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

run_bounded "migrate cross-app acceptance database" "${INSTANT_CROSS_APP_MIGRATE_TIMEOUT:-5m}" \
  "${compose[@]}" run --rm --no-deps server migrate up
"${compose[@]}" up -d --no-build server

for _ in $(seq 1 90); do
  if curl --silent --show-error --fail --max-time 2 \
      -X POST "$server_origin/dash/apps/ephemeral" \
      -H 'content-type: application/json' \
      --data '{"title":"cross-app-isolation-readiness"}' >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

if ! curl --silent --show-error --fail --max-time 2 \
    -X POST "$server_origin/dash/apps/ephemeral" \
    -H 'content-type: application/json' \
    --data '{"title":"cross-app-isolation-readiness"}' >/dev/null; then
  echo 'Rust server did not become ready within 180 seconds' >&2
  exit 1
fi

run_bounded "cross-app isolation acceptance" "${INSTANT_CROSS_APP_ACCEPTANCE_TIMEOUT:-5m}" \
  env INSTANT_API_ORIGIN="$server_origin" INSTANT_S3_ORIGIN="$s3_origin" \
    "${node_cmd[@]}" "$repo_root/self-hosting/cross-app-isolation-acceptance.mjs"

echo "Cross-app isolation acceptance passed"
