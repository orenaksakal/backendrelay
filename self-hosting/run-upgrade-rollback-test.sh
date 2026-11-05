#!/usr/bin/env bash
set -Eeuo pipefail

# Upgrade and rollback acceptance across two server images.
#
# 1. Start the previous image, migrate, and seed an app through the admin API.
# 2. Run `migrate up` with the candidate image.
# 3. Start the candidate and run the Admin SDK smoke.
# 4. Start the previous image against the newer schema. Additive migrations
#    must serve (the P3.9 compatibility floor); anything else must refuse.
# 5. Run `migrate down` for exactly the migrations the candidate added.
# 6. Start the previous image again and verify the seeded data plus a fresh
#    write and realtime delivery.
#
# run-official-sdk-e2e.sh always creates its own Compose stack, so it cannot
# target this one. The smoke here is the Admin SDK read/write/realtime check in
# upgrade-rollback-smoke.mjs.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compose_file="$repo_root/self-hosting/docker-compose.test.yml"
smoke_script="$repo_root/self-hosting/upgrade-rollback-smoke.mjs"
from_tag=backendrelay/server:upgrade-from
to_tag=backendrelay/server:upgrade-to
from_image=
to_image=
from_ref=HEAD~1
from_ref_set=false
images_mode=false
readiness_seconds=${INSTANT_UPGRADE_READINESS_SECONDS:-180}
seed_entities=${INSTANT_UPGRADE_SEED_ENTITIES:-3}
project=instant-upgrade-rollback-$$-$RANDOM
database=${POSTGRES_DB:-instant}

usage() {
  cat <<'EOF'
usage: run-upgrade-rollback-test.sh [--from-ref REF | --from-image IMAGE --to-image IMAGE]

  --from-ref REF     Build the previous server image from REF and the candidate
                     image from the current working tree (default: HEAD~1).
  --from-image IMAGE Use IMAGE as the previous server image (requires --to-image).
  --to-image IMAGE   Use IMAGE as the candidate server image.

Built images are tagged backendrelay/server:upgrade-from and
backendrelay/server:upgrade-to. The script uses its own Compose project, random
host ports, and deletes the project volumes when it exits.

Environment:
  INSTANT_UPGRADE_SEED_ENTITIES          seeded rows (default 3)
  INSTANT_UPGRADE_READINESS_SECONDS      server readiness limit (default 180)
  INSTANT_UPGRADE_BUILD_TIMEOUT          image build limit (default 30m)
  INSTANT_UPGRADE_MIGRATE_TIMEOUT        migrate limit (default 5m)
  INSTANT_UPGRADE_SMOKE_TIMEOUT          Admin SDK smoke limit (default 5m)
  INSTANT_UPGRADE_SMOKE_TIMEOUT_MS       Admin SDK socket timeout (default 60000)
  INSTANT_UPGRADE_SKIP_INSTALL=1         reuse installed client dependencies
  INSTANT_POSTGRES_IMAGE                 pull a PostgreSQL image instead of building
EOF
}

while (($#)); do
  case "$1" in
    --from-image)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      from_image=$2
      images_mode=true
      shift 2
      ;;
    --to-image)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      to_image=$2
      images_mode=true
      shift 2
      ;;
    --from-ref)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      from_ref=$2
      from_ref_set=true
      shift 2
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

if [[ $images_mode == true ]]; then
  [[ -n $from_image && -n $to_image ]] || {
    echo 'Both --from-image and --to-image are required when using images.' >&2
    exit 2
  }
  [[ $from_ref_set == false ]] || {
    echo '--from-ref cannot be combined with --from-image/--to-image.' >&2
    exit 2
  }
else
  from_image=$from_tag
  to_image=$to_tag
fi
[[ $from_image != "$to_image" ]] || {
  echo 'The previous and candidate server images must differ.' >&2
  exit 2
}
[[ $readiness_seconds =~ ^[1-9][0-9]*$ ]] || {
  echo 'INSTANT_UPGRADE_READINESS_SECONDS must be a positive integer.' >&2
  exit 2
}
[[ $seed_entities =~ ^[1-9][0-9]*$ ]] || {
  echo 'INSTANT_UPGRADE_SEED_ENTITIES must be a positive integer.' >&2
  exit 2
}

for command in docker curl node python3 git; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done
docker compose version >/dev/null
if command -v mise >/dev/null 2>&1; then
  pnpm_cmd=(mise exec node@24 pnpm@10.34.9 -- pnpm)
else
  command -v pnpm >/dev/null 2>&1 || {
    echo 'required command is not installed: pnpm' >&2
    exit 1
  }
  pnpm_cmd=(pnpm)
fi

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
export INSTANT_SERVER_PORT=${INSTANT_SERVER_PORT:-${available_ports[0]}}
export INSTANT_POSTGRES_PORT=${INSTANT_POSTGRES_PORT:-${available_ports[1]}}
export INSTANT_S3_TEST_PORT=${INSTANT_S3_TEST_PORT:-${available_ports[2]}}
for port in "$INSTANT_SERVER_PORT" "$INSTANT_POSTGRES_PORT" "$INSTANT_S3_TEST_PORT"; do
  [[ $port =~ ^[1-9][0-9]{0,4}$ ]] && ((port <= 65535)) || {
    echo 'Acceptance ports must be valid nonzero TCP ports.' >&2
    exit 2
  }
done
export S3_PUBLIC_ENDPOINT=${S3_PUBLIC_ENDPOINT:-http://localhost:$INSTANT_S3_TEST_PORT}
server_origin="http://127.0.0.1:$INSTANT_SERVER_PORT"
compose=(docker compose --project-name "$project" --file "$compose_file")

run_root=$(mktemp -d "${TMPDIR:-/tmp}/instant-upgrade-rollback.XXXXXX")
worktree_dir="$run_root/from-worktree"
owns_project=false
timer_start=$SECONDS
seed_marker=

report() {
  printf 'step=%s result=%s detail=%s\n' "$1" "$2" "$3"
}

fail_step() {
  report "$1" fail "$2"
  exit 1
}

compose_image() {
  local image=$1
  shift
  INSTANT_SERVER_IMAGE="$image" "${compose[@]}" "$@"
}

replace_server() {
  compose_image "$1" up -d --no-deps --no-build --force-recreate server
}

server_logs() {
  "${compose[@]}" logs --tail 80 server >&2 || true
}

wait_postgres() {
  local deadline=$((SECONDS + readiness_seconds))
  while ((SECONDS < deadline)); do
    if "${compose[@]}" exec -T postgres pg_isready --username postgres --dbname "$database" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_server() {
  local deadline=$((SECONDS + readiness_seconds))
  local container
  while ((SECONDS < deadline)); do
    if "${compose[@]}" exec -T server /usr/local/bin/instant-server healthcheck >/dev/null 2>&1; then
      return 0
    fi
    if curl --silent --show-error --fail --max-time 2 "$server_origin/health/system" >/dev/null 2>&1; then
      return 0
    fi
    container=$("${compose[@]}" ps -q server)
    if [[ -n $container ]] &&
        [[ $(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null || true) == false ]]; then
      return 1
    fi
    sleep 1
  done
  return 1
}

db_query() {
  "${compose[@]}" exec -T postgres psql --username instant --dbname "$database" \
    --no-psqlrc --tuples-only --no-align --quiet --command "$1"
}

db_version() {
  db_query 'SELECT version FROM schema_migrations LIMIT 1'
}

run_smoke() {
  local stage=$1
  run_bounded "$stage Admin SDK read/write/realtime smoke" \
    "${INSTANT_UPGRADE_SMOKE_TIMEOUT:-5m}" \
    env INSTANT_UPGRADE_API_URL="$server_origin" \
      INSTANT_UPGRADE_APP_ID="$app_id" \
      INSTANT_UPGRADE_ADMIN_TOKEN="$admin_token" \
      INSTANT_UPGRADE_STAGE="$stage" \
      INSTANT_UPGRADE_SEED_MARKER="$seed_marker" \
      INSTANT_UPGRADE_MIN_SEEDED_ROWS="$seed_entities" \
      INSTANT_UPGRADE_SMOKE_TIMEOUT_MS="${INSTANT_UPGRADE_SMOKE_TIMEOUT_MS:-60000}" \
      node "$smoke_script"
}

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
    printf 'total_seconds=%s\n' "$((SECONDS - timer_start))" >&2
  fi
  if [[ $owns_project == true ]]; then
    INSTANT_SERVER_IMAGE="$to_image" "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  if [[ -d $worktree_dir ]]; then
    git -C "$repo_root" worktree remove --force "$worktree_dir" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$run_root"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ -n $("${compose[@]}" ps -aq) ]] ||
    [[ -n $(docker volume ls --quiet --filter "label=com.docker.compose.project=$project") ]]; then
  echo 'Refusing to use a Compose project that already owns containers or volumes.' >&2
  exit 1
fi
owns_project=true

if [[ $images_mode == false ]]; then
  if ! git -C "$repo_root" rev-parse --verify --quiet "$from_ref^{commit}" >/dev/null; then
    fail_step 0 "previous ref is not a commit: $from_ref"
  fi
  if ! git -C "$repo_root" worktree add --detach "$worktree_dir" "$from_ref" >/dev/null; then
    fail_step 0 "could not create a worktree for $from_ref"
  fi
  from_sha=$(git -C "$repo_root" rev-parse --short=12 "$from_ref^{commit}")
  to_sha=$(git -C "$repo_root" rev-parse --short=12 HEAD)
  if ! run_bounded "build previous server image ($from_ref)" \
      "${INSTANT_UPGRADE_BUILD_TIMEOUT:-30m}" \
      docker build --tag "$from_image" --build-arg OCI_REVISION="$from_sha" \
        "$worktree_dir/server-rs"; then
    fail_step 0 "previous server image build failed for $from_ref"
  fi
  if ! run_bounded "build candidate server image (working tree)" \
      "${INSTANT_UPGRADE_BUILD_TIMEOUT:-30m}" \
      docker build --tag "$to_image" --build-arg OCI_REVISION="$to_sha" \
        "$repo_root/server-rs"; then
    fail_step 0 'candidate server image build failed'
  fi
fi

if [[ ${INSTANT_UPGRADE_SKIP_INSTALL:-0} != 1 ]]; then
  if ! run_bounded "install official SDK workspace dependencies" \
      "${INSTANT_UPGRADE_INSTALL_TIMEOUT:-10m}" \
      "${pnpm_cmd[@]}" --dir "$repo_root/client" install --frozen-lockfile --prefer-offline; then
    fail_step 0 'client dependency install failed'
  fi
fi
if ! run_bounded "build official Admin SDK" "${INSTANT_UPGRADE_SDK_BUILD_TIMEOUT:-10m}" \
    "${pnpm_cmd[@]}" --dir "$repo_root/client" exec turbo run build \
      --filter=@instantdb/admin --cache-dir=.turbo --no-update-notifier; then
  fail_step 0 'Admin SDK build failed'
fi

if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  run_bounded "pull pinned PostgreSQL image" "${INSTANT_UPGRADE_BUILD_TIMEOUT:-30m}" \
    "${compose[@]}" pull postgres || fail_step 0 'PostgreSQL image pull failed'
else
  run_bounded "build local PostgreSQL image" "${INSTANT_UPGRADE_BUILD_TIMEOUT:-30m}" \
    "${compose[@]}" build postgres || fail_step 0 'PostgreSQL image build failed'
fi
"${compose[@]}" up -d --no-build postgres s3 createbuckets ||
  fail_step 0 'could not start PostgreSQL/S3 test services'
wait_postgres || fail_step 0 'PostgreSQL did not become ready'
if [[ $images_mode == false ]]; then
  report 0 pass "built from=$from_image ($from_ref) to=$to_image (working tree)"
else
  report 0 pass "using supplied images from=$from_image to=$to_image"
fi

# 1. Start the previous image, migrate, and seed through the admin API.
if ! run_bounded "previous image migrate up" "${INSTANT_UPGRADE_MIGRATE_TIMEOUT:-5m}" \
    env INSTANT_SERVER_IMAGE="$from_image" "${compose[@]}" run --rm --no-deps server migrate up \
    >"$run_root/from-migrate.log" 2>&1; then
  cat "$run_root/from-migrate.log" >&2 || true
  fail_step 1 'previous image migrate up failed'
fi
if ! from_version=$(db_version); then
  fail_step 1 'could not read the schema version after the previous image migration'
fi
replace_server "$from_image"
if ! wait_server; then
  server_logs
  fail_step 1 'previous image did not become healthy before seeding'
fi

if ! curl --silent --show-error --fail --max-time 10 --output "$run_root/app.json" \
    -X POST "$server_origin/dash/apps/ephemeral" \
    -H 'content-type: application/json' \
    --data '{"title":"upgrade-rollback-acceptance"}'; then
  fail_step 1 'could not create the acceptance app'
fi
if ! app_id=$(python3 - "$run_root/app.json" <<'PY'
import json
import pathlib
import sys
import uuid

app = json.loads(pathlib.Path(sys.argv[1]).read_text()).get("app", {})
app_id = app.get("id", "")
uuid.UUID(app_id)
print(app_id)
PY
); then
  fail_step 1 'acceptance app response did not include a valid app id'
fi
if ! admin_token=$(python3 - "$run_root/app.json" <<'PY'
import json
import pathlib
import sys

token = json.loads(pathlib.Path(sys.argv[1]).read_text()).get("app", {}).get("admin-token", "")
if not token:
    raise SystemExit("acceptance app response did not include an admin token")
print(token)
PY
); then
  fail_step 1 'acceptance app response did not include an admin token'
fi

seed_marker="upgrade-rollback-seed-$$-$RANDOM"
if ! seed_payload=$(python3 - "$seed_entities" "$seed_marker" <<'PY'
import json
import sys
import uuid

count = int(sys.argv[1])
marker = sys.argv[2]
entity_attr = "10000000-0000-4000-8000-000000000001"
marker_attr = "10000000-0000-4000-8000-000000000002"
seq_attr = "10000000-0000-4000-8000-000000000003"
steps = [
    ["add-attr", {"id": entity_attr, "forward-identity": [str(uuid.uuid4()), "upgrade_rollback_items", "id"], "value-type": "blob", "cardinality": "one", "unique?": True, "index?": True}],
    ["add-attr", {"id": marker_attr, "forward-identity": [str(uuid.uuid4()), "upgrade_rollback_items", "marker"], "value-type": "blob", "cardinality": "one", "unique?": False, "index?": False}],
    ["add-attr", {"id": seq_attr, "forward-identity": [str(uuid.uuid4()), "upgrade_rollback_items", "seq"], "value-type": "blob", "cardinality": "one", "unique?": False, "index?": False}],
]
for index in range(count):
    row = str(uuid.uuid4())
    steps.append(["add-triple", row, entity_attr, row])
    steps.append(["add-triple", row, marker_attr, marker])
    steps.append(["add-triple", row, seq_attr, str(index)])
print(json.dumps({"steps": steps}))
PY
); then
  fail_step 1 'could not build the seed transaction'
fi
if ! curl --silent --show-error --fail --max-time 10 --output "$run_root/seed.json" \
    -X POST "$server_origin/admin/transact" \
    -H 'content-type: application/json' \
    -H "app-id: $app_id" \
    -H "authorization: Bearer $admin_token" \
    --data "$seed_payload"; then
  fail_step 1 'previous image rejected the seed transaction'
fi
if ! curl --silent --show-error --fail --max-time 10 --output "$run_root/seed-query.json" \
    -X POST "$server_origin/admin/query" \
    -H 'content-type: application/json' \
    -H "app-id: $app_id" \
    -H "authorization: Bearer $admin_token" \
    --data '{"query":{"upgrade_rollback_items":{}}}'; then
  fail_step 1 'previous image could not read the seeded rows'
fi
if ! python3 - "$run_root/seed-query.json" "$seed_marker" "$seed_entities" <<'PY'
import json
import pathlib
import sys

body = json.loads(pathlib.Path(sys.argv[1]).read_text())
rows = body.get("upgrade_rollback_items") or []
seeded = [row for row in rows if row.get("marker") == sys.argv[2]]
if len(seeded) < int(sys.argv[3]):
    raise SystemExit(f"seeded row read returned {len(seeded)} rows, expected {sys.argv[3]}")
PY
then
  fail_step 1 'seeded row read did not match the seed transaction'
fi
report 1 pass "previous=${from_image} schema=${from_version} seeded=${seed_entities}"

# 2. Apply the candidate migrations.
if ! run_bounded "candidate image migrate up" "${INSTANT_UPGRADE_MIGRATE_TIMEOUT:-5m}" \
    env INSTANT_SERVER_IMAGE="$to_image" "${compose[@]}" run --rm --no-deps server migrate up \
    >"$run_root/to-migrate.log" 2>&1; then
  cat "$run_root/to-migrate.log" >&2 || true
  fail_step 2 'candidate image migrate up failed'
fi
if ! to_version=$(db_version); then
  fail_step 2 'could not read the schema version after the candidate migration'
fi
if ((to_version < from_version)); then
  fail_step 2 "candidate schema $to_version is older than the previous schema $from_version"
fi
new_migrations=$((to_version - from_version))
report 2 pass "candidate=${to_image} schema=${from_version}->${to_version} new_migrations=${new_migrations}"

# 3. Start the candidate and smoke it.
replace_server "$to_image"
if ! wait_server; then
  server_logs
  fail_step 3 'candidate image did not become healthy'
fi
if ! run_smoke candidate; then
  server_logs
  fail_step 3 'candidate Admin SDK smoke failed'
fi
report 3 pass "candidate served schema=${to_version} after migrate up"

# 4. Start the previous image against the newer schema.
additive_ok=true
if ((new_migrations > 0)); then
  if ! range_state=$(db_query "SELECT count(*), coalesce(bool_and(additive), false) FROM instant_migration_checksums WHERE version > $from_version AND version <= $to_version"); then
    fail_step 4 'could not read the additive flags for the new migrations'
  fi
  IFS='|' read -r range_count range_additive <<<"$range_state"
  [[ $range_count == "$new_migrations" ]] ||
    fail_step 4 "recorded additive flags ($range_count) do not match the new migrations ($new_migrations)"
  [[ $range_additive == t ]] || additive_ok=false
fi
replace_server "$from_image"
if wait_server; then
  if [[ $additive_ok == false ]]; then
    server_logs
    fail_step 4 'previous image served a schema whose new migrations are not all additive'
  fi
  if ! run_smoke from-after-upgrade; then
    server_logs
    fail_step 4 'previous image Admin SDK smoke failed against the additive schema'
  fi
  report 4 pass "previous image served additive schema=${to_version} from=${from_version}"
else
  server_logs
  if [[ $additive_ok == true ]]; then
    fail_step 4 'previous image did not serve an additive schema'
  fi
  report 4 pass 'non-additive: rollback requires migrate down'
fi
"${compose[@]}" stop server >/dev/null 2>&1 || true

# 5. Roll back exactly the migrations the candidate added.
if ((new_migrations == 0)); then
  report 5 pass 'no new migrations to roll back'
else
  if ! run_bounded "candidate image migrate down" "${INSTANT_UPGRADE_MIGRATE_TIMEOUT:-5m}" \
      env INSTANT_SERVER_IMAGE="$to_image" "${compose[@]}" run --rm --no-deps server \
      migrate down "$new_migrations" >"$run_root/down-migrate.log" 2>&1; then
    cat "$run_root/down-migrate.log" >&2 || true
    fail_step 5 "candidate image migrate down $new_migrations failed"
  fi
  if ! rolled_back_version=$(db_version); then
    fail_step 5 'could not read the schema version after migrate down'
  fi
  [[ $rolled_back_version == "$from_version" ]] ||
    fail_step 5 "schema after migrate down is $rolled_back_version, expected $from_version"
  report 5 pass "rolled back ${new_migrations} migrations ${to_version}->${rolled_back_version}"
fi

# 6. Start the previous image again and verify the seeded data and a fresh write.
replace_server "$from_image"
if ! wait_server; then
  server_logs
  fail_step 6 'previous image did not become healthy after migrate down'
fi
if ! run_smoke from-after-rollback; then
  server_logs
  fail_step 6 'previous image Admin SDK smoke failed after migrate down'
fi
report 6 pass "previous image served rolled-back schema=${from_version} with seeded data"

printf 'total_seconds=%s\n' "$((SECONDS - timer_start))"
echo 'Upgrade and rollback acceptance passed: both images migrated, the previous image served additive schema, and migrate down restored a working previous image with the seeded data.'
