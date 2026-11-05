#!/usr/bin/env bash
set -Eeuo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
compose_file="$repo_root/self-hosting/docker-compose.test.yml"
candidate=${INSTANT_SERVER_IMAGE:-}
baseline=${INSTANT_ROLLBACK_SERVER_IMAGE:-}
readiness_seconds=${COMPOSE_ROLLBACK_READINESS_SECONDS:-180}
[[ -z ${COMPOSE_ROLLBACK_PROJECT:-} ]] || {
  echo 'COMPOSE_ROLLBACK_PROJECT is not supported; this test always creates an isolated project.' >&2
  exit 2
}
project=instant-compose-rollback-$$-$RANDOM
title=compose-image-rollback-acceptance

[[ ${COMPOSE_ROLLBACK_CONFIRM:-} == replace-candidate-with-baseline-and-restore-candidate ]] || {
  echo 'Set COMPOSE_ROLLBACK_CONFIRM=replace-candidate-with-baseline-and-restore-candidate to run the live acceptance test.' >&2
  exit 2
}
for image in "$candidate" "$baseline"; do
  "$repo_root/self-hosting/verify-release-images.sh" --shape-only "$image" >/dev/null
done
[[ $candidate != "$baseline" ]] || { echo 'Candidate and baseline server images must differ.' >&2; exit 2; }
[[ $readiness_seconds =~ ^[1-9][0-9]*$ ]] || { echo 'COMPOSE_ROLLBACK_READINESS_SECONDS must be a positive integer.' >&2; exit 2; }

for command in docker curl python3; do
  command -v "$command" >/dev/null 2>&1 || { echo "required command is not installed: $command" >&2; exit 1; }
done
docker compose version >/dev/null

run_root=$(mktemp -d "${TMPDIR:-/tmp}/instant-compose-rollback.XXXXXX")
create_response="$run_root/create.json"
read_response="$run_root/read.json"
transaction_response="$run_root/transaction.json"
query_response="$run_root/query.json"
app_id=
admin_token=
postgres_container=
postgres_volume=
baseline_attempted=false
candidate_restored=false
owns_project=false

acceptance_ports=()
while IFS= read -r acceptance_port; do
  acceptance_ports+=("$acceptance_port")
done < <(python3 - <<'PY'
import socket

listeners = [socket.socket() for _ in range(2)]
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

INSTANT_SERVER_PORT=${acceptance_ports[0]}
INSTANT_POSTGRES_PORT=${acceptance_ports[1]}
export INSTANT_SERVER_PORT INSTANT_POSTGRES_PORT
export S3_PUBLIC_ENDPOINT=${S3_PUBLIC_ENDPOINT:-http://localhost:9000}
server_origin="http://127.0.0.1:$INSTANT_SERVER_PORT"
compose=(docker compose --project-name "$project" -f "$compose_file")

compose_image() {
  local image=$1
  shift
  INSTANT_SERVER_IMAGE="$image" "${compose[@]}" "$@"
}

replace_server() {
  compose_image "$1" up -d --no-deps --no-build --force-recreate server
}

wait_server() {
  local deadline=$((SECONDS + readiness_seconds))
  while ((SECONDS < deadline)); do
    if "${compose[@]}" exec -T server /usr/local/bin/instant-server healthcheck >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_postgres() {
  local deadline=$((SECONDS + readiness_seconds))
  while ((SECONDS < deadline)); do
    if "${compose[@]}" exec -T postgres pg_isready --username postgres --dbname instant >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

postgres_mount() {
  docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/postgresql/data"}}{{.Name}}{{end}}{{end}}' "$1"
}

assert_postgres_unchanged() {
  local current_container current_volume
  current_container=$("${compose[@]}" ps -q postgres)
  current_volume=$(postgres_mount "$current_container")
  [[ -n $current_container && $current_container == "$postgres_container" ]] || {
    echo 'PostgreSQL was replaced during the server-image rollback.' >&2
    return 1
  }
  [[ -n $current_volume && $current_volume == "$postgres_volume" ]] || {
    echo 'The PostgreSQL data volume changed during the server-image rollback.' >&2
    return 1
  }
}

fetch_existing_app() {
  curl --silent --show-error --fail --max-time 5 --output "$read_response" \
    "$server_origin/dash/apps/ephemeral/$app_id" || return 1
  python3 - "$read_response" "$app_id" "$title" <<'PY'
import json
import pathlib
import sys

body = json.loads(pathlib.Path(sys.argv[1]).read_text())
app = body.get("app", {})
if app.get("id") != sys.argv[2] or app.get("title") != sys.argv[3]:
    raise SystemExit("existing app read did not match the candidate-created record")
PY
}

fetch_application_data() {
  curl --silent --show-error --fail --max-time 5 --output "$query_response" \
    -X POST "$server_origin/admin/query" \
    -H 'content-type: application/json' \
    -H "app-id: $app_id" \
    -H "authorization: Bearer $admin_token" \
    --data '{"query":{"rollback_checks":{}}}' || return 1
  python3 - "$query_response" <<'PY'
import json
import pathlib
import sys

body = json.loads(pathlib.Path(sys.argv[1]).read_text())
expected = [{"id": "66666666-6666-4666-8666-666666666666", "marker": "candidate-data"}]
if body.get("rollback_checks") != expected:
    raise SystemExit("application-data read did not match the candidate-created record")
PY
}

finish() {
  local status=$?
  trap - EXIT INT TERM
  if ((status != 0)) && [[ $baseline_attempted == true && $candidate_restored == false ]]; then
    if replace_server "$candidate" >/dev/null 2>&1 && wait_server && assert_postgres_unchanged && \
        fetch_existing_app && fetch_application_data; then
      printf '%s\n' 'failure_recovery=candidate_observed' >&2
    else
      printf '%s\n' 'failure_recovery=failed' >&2
    fi
  fi
  if [[ $owns_project == true ]]; then
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  rm -rf -- "$run_root"
  exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ -n $("${compose[@]}" ps -aq) ]] ||
    [[ -n $(docker volume ls --quiet --filter "label=com.docker.compose.project=$project") ]]; then
  echo 'Refusing to use a Compose project that already owns containers or volumes.' >&2
  exit 1
fi
owns_project=true

compose_image "$candidate" pull server
compose_image "$baseline" pull server
if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  "${compose[@]}" pull postgres
else
  "${compose[@]}" build postgres
fi
"${compose[@]}" up -d --no-build postgres
postgres_container=$("${compose[@]}" ps -q postgres)
[[ -n $postgres_container ]] || { echo 'Compose did not start PostgreSQL.' >&2; exit 1; }
postgres_volume=$(postgres_mount "$postgres_container")
[[ -n $postgres_volume ]] || { echo 'PostgreSQL does not have its named data volume mounted.' >&2; exit 1; }
wait_postgres || { echo 'PostgreSQL did not become ready before candidate migration.' >&2; exit 1; }

compose_image "$candidate" run --rm --no-deps server migrate up
replace_server "$candidate"
wait_server || { echo 'Candidate server did not become healthy after migration.' >&2; exit 1; }
assert_postgres_unchanged

curl --silent --show-error --fail --max-time 10 --output "$create_response" \
  -X POST "$server_origin/dash/apps/ephemeral" \
  -H 'content-type: application/json' \
  --data "{\"title\":\"$title\"}"
app_id=$(python3 - "$create_response" <<'PY'
import json
import pathlib
import sys
import uuid

app_id = json.loads(pathlib.Path(sys.argv[1]).read_text()).get("app", {}).get("id", "")
uuid.UUID(app_id)
print(app_id)
PY
)
admin_token=$(python3 - "$create_response" <<'PY'
import json
import pathlib
import sys

token = json.loads(pathlib.Path(sys.argv[1]).read_text()).get("app", {}).get("admin-token", "")
if not token:
    raise SystemExit("candidate-created app did not include an admin token")
print(token)
PY
)
fetch_existing_app

curl --silent --show-error --fail --max-time 10 --output "$transaction_response" \
  -X POST "$server_origin/admin/transact" \
  -H 'content-type: application/json' \
  -H "app-id: $app_id" \
  -H "authorization: Bearer $admin_token" \
  --data '{"steps":[["add-attr",{"id":"22222222-2222-4222-8222-222222222222","forward-identity":["33333333-3333-4333-8333-333333333333","rollback_checks","id"],"value-type":"blob","cardinality":"one","unique?":true,"index?":true}],["add-attr",{"id":"44444444-4444-4444-8444-444444444444","forward-identity":["55555555-5555-4555-8555-555555555555","rollback_checks","marker"],"value-type":"blob","cardinality":"one","unique?":false,"index?":false}],["add-triple","66666666-6666-4666-8666-666666666666","22222222-2222-4222-8222-222222222222","66666666-6666-4666-8666-666666666666"],["add-triple","66666666-6666-4666-8666-666666666666","44444444-4444-4444-8444-444444444444","candidate-data"]]}'
fetch_application_data

baseline_attempted=true
replace_server "$baseline"
if ! wait_server || ! assert_postgres_unchanged || ! fetch_existing_app || ! fetch_application_data; then
  echo 'Rollback cannot prove baseline compatibility with the candidate-migrated database and existing data.' >&2
  exit 1
fi

replace_server "$candidate"
if ! wait_server || ! assert_postgres_unchanged || ! fetch_existing_app || ! fetch_application_data; then
  echo 'Candidate server did not recover its existing read after rollback restoration.' >&2
  exit 1
fi
candidate_restored=true

echo 'Compose server-image rollback acceptance passed: candidate migration, baseline existing read, and candidate restoration used one PostgreSQL container and data volume.'
