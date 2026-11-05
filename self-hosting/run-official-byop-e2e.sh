#!/usr/bin/env bash
set -Eeuo pipefail

# Proves the endpoint-configurable official Admin SDK can query a real external
# PostgreSQL schema and receive a refresh from its wal2json commit. This is a
# hermetic acceptance fixture: it never contacts Instant Cloud or Cloudflare.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compose_file="$repo_root/self-hosting/docker-compose.test.yml"
export COMPOSE_PROJECT_NAME=instant-byop-e2e
export POSTGRES_USER=postgres
export POSTGRES_PASSWORD=pass
export POSTGRES_DB=instant
export INSTANT_DB_USER=instant
export INSTANT_DB_PASSWORD=instant-test
compose=(docker compose -f "$compose_file")
server_image=${INSTANT_SERVER_IMAGE:-}

if command -v mise >/dev/null 2>&1; then
  pnpm_cmd=(mise exec node@24 pnpm@10.34.9 -- pnpm)
else
  pnpm_cmd=(pnpm)
fi

if [[ -n $server_image ]]; then
  "$repo_root/self-hosting/verify-release-images.sh" --shape-only "$server_image"
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

redact_logs() {
  sed -E \
    -e 's#(postgres(ql)?://[^:/@[:space:]]+):[^@[:space:]]+@#\1:[REDACTED]@#g' \
    -e 's#(Bearer )[A-Za-z0-9._~+/=-]+#\1[REDACTED]#g' \
    -e 's#("(admin-token|token|password|secret|secret-key|access-key)"[[:space:]]*:[[:space:]]*")[^"]*"#\1[REDACTED]"#g' \
    -e 's#((AEAD|WEBHOOK)_KEYSET_JSON=)[^[:space:]]+#\1[REDACTED]#g'
}

cleanup() {
  status=$?
  trap - EXIT
  if [ -n "${mutator_pid:-}" ]; then
    kill "$mutator_pid" 2>/dev/null || true
    wait "$mutator_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    # Bound and redact known credential shapes before preserving CI diagnostics.
    # Docker logs do not print container environment variables.
    "${compose[@]}" logs --no-color --tail=500 server postgres \
      | redact_logs >&2 || true
  fi
  "${compose[@]}" down --volumes --remove-orphans || true
  exit "$status"
}
trap cleanup EXIT

"${compose[@]}" down --volumes --remove-orphans
if [[ -n $server_image ]]; then
  run_bounded "pull candidate Rust server image" "${INSTANT_BYOP_E2E_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull server
else
  run_bounded "build local Rust server image" "${INSTANT_BYOP_E2E_BUILD_TIMEOUT:-20m}" \
    "${compose[@]}" build server
fi
if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  run_bounded "pull pinned PostgreSQL image" "${INSTANT_BYOP_E2E_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull postgres
else
  run_bounded "build local PostgreSQL image" "${INSTANT_BYOP_E2E_BUILD_TIMEOUT:-20m}" \
    "${compose[@]}" build postgres
fi
S3_PUBLIC_ENDPOINT=http://localhost:9000 "${compose[@]}" up -d --no-build \
  postgres s3 createbuckets
postgres_ready_checks=0
for _ in $(seq 1 60); do
  if "${compose[@]}" exec -T postgres pg_isready -U postgres -d instant >/dev/null 2>&1; then
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
run_bounded "migrate BYOP fixture database" "${INSTANT_BYOP_E2E_MIGRATE_TIMEOUT:-5m}" \
  env S3_PUBLIC_ENDPOINT=http://localhost:9000 \
    "${compose[@]}" run --rm --no-deps server migrate up
S3_PUBLIC_ENDPOINT=http://localhost:9000 "${compose[@]}" up -d --no-build server

for _ in $(seq 1 90); do
  if app_json=$(curl --silent --show-error --fail --max-time 2 \
      -X POST http://localhost:8888/dash/apps/ephemeral \
      -H 'content-type: application/json' \
      --data '{"title":"official-client-byop-e2e","schema":{"entities":{"$users":{"attrs":{"username":{"metadata":{},"valueType":"string","required":false,"isIndexed":true,"config":{"indexed":true,"unique":true}}},"links":{}},"teams":{"attrs":{"slug":{"metadata":{},"valueType":"string","required":true,"isIndexed":true,"config":{"indexed":true,"unique":true}}},"links":{"members":{"cardinality":"many","entityName":"$users"}}}},"links":{"userTeams":{"forward":{"on":"$users","label":"teams","has":"many"},"reverse":{"on":"teams","label":"members","has":"many"}}},"rooms":{}},"rules":{"code":{"$users":{"allow":{"create":"true","update":"auth.id == data.id","link":{"teams":"auth.id == data.id"},"unlink":{"teams":"auth.id == data.id"}}},"teams":{"allow":{"create":"auth.id != null","update":"auth.id != null","link":{"members":"auth.id != null"},"unlink":{"members":"auth.id != null"}}},"todos":{"allow":{"view":"data.public || (auth.id != null && auth.id in data.ref('"'"'owners.accounts.user_id'"'"'))"}},"owners":{"allow":{"view":"auth.id != null && auth.id in data.ref('"'"'accounts.user_id'"'"')"}},"profiles":{"allow":{"view":"auth.id != null && auth.id in data.ref('"'"'accounts.user_id'"'"')"}},"accounts":{"allow":{"view":"auth.id != null && data.ref('"'"'profiles.viewer_alias'"'"')[0] in auth.ref('"'"'$user.teams.slug'"'"') && auth.id in data.ref('"'"'owners.todos.viewer_id'"'"')"}}}}}' 2>/dev/null); then
    break
  fi
  sleep 2
done

if [ -z "${app_json:-}" ]; then
  echo 'Rust server did not become ready within 180 seconds' >&2
  exit 1
fi

app_id=$(node -e 'const chunks=[]; process.stdin.on("data", c => chunks.push(c)); process.stdin.on("end", () => process.stdout.write(JSON.parse(Buffer.concat(chunks)).app.id));' <<<"$app_json")
admin_token=$(node -e 'const chunks=[]; process.stdin.on("data", c => chunks.push(c)); process.stdin.on("end", () => process.stdout.write(JSON.parse(Buffer.concat(chunks)).app["admin-token"]));' <<<"$app_json")
schema="byop_e2e_${app_id//-/}"

"${compose[@]}" exec -T postgres psql -U instant -d instant -v ON_ERROR_STOP=1 <<SQL
CREATE SCHEMA $schema;
CREATE TABLE $schema.accounts (
  id uuid PRIMARY KEY,
  user_id uuid UNIQUE NOT NULL
);
CREATE TABLE $schema.profiles (
  id uuid PRIMARY KEY,
  account_id uuid UNIQUE NOT NULL REFERENCES $schema.accounts(id) ON DELETE CASCADE,
  viewer_alias text NOT NULL
);
CREATE TABLE $schema.owners (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES $schema.accounts(id) ON DELETE CASCADE
);
CREATE INDEX owners_account_id_idx ON $schema.owners(account_id);
CREATE TABLE $schema.todos (
  id uuid PRIMARY KEY,
  title text NOT NULL,
  public boolean NOT NULL DEFAULT false,
  owner_id uuid REFERENCES $schema.owners(id) ON DELETE CASCADE,
  viewer_id uuid,
  viewer_alias text,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX todos_owner_id_idx ON $schema.todos(owner_id);
INSERT INTO $schema.todos (id, title, public) VALUES
  ('00000000-0000-4000-8000-000000000001', 'seed', true),
  ('00000000-0000-4000-8000-000000000002', 'private', false);
INSERT INTO $schema.accounts (id, user_id) VALUES
  ('00000000-0000-4000-8000-000000000401', '00000000-0000-4000-8000-000000000098');
INSERT INTO $schema.profiles (id, account_id, viewer_alias) VALUES
  ('00000000-0000-4000-8000-000000000501', '00000000-0000-4000-8000-000000000401', 'seed-profile');
SQL

byop_url="postgresql://${INSTANT_DB_USER}:${INSTANT_DB_PASSWORD}@postgres:5432/instant?sslmode=disable&currentSchema=$schema"
printf '%s\n' "$byop_url" | "${compose[@]}" exec -T server \
  /usr/local/bin/docker-entrypoint.sh configure-byop "$app_id"
unset byop_url

slot_name="instant_byop_${app_id//-/}"
for _ in $(seq 1 45); do
  if "${compose[@]}" exec -T postgres psql -U instant -d instant -Atqc \
      "SELECT 1 FROM pg_replication_slots WHERE slot_name = '$slot_name' AND plugin = 'wal2json'" \
      | grep -qx 1; then
    break
  fi
  sleep 2
done
if ! "${compose[@]}" exec -T postgres psql -U instant -d instant -Atqc \
    "SELECT 1 FROM pg_replication_slots WHERE slot_name = '$slot_name' AND plugin = 'wal2json'" \
    | grep -qx 1; then
  echo "Rust BYOP invalidator did not create $slot_name within 90 seconds" >&2
  exit 1
fi

if [[ ${pnpm_cmd[0]} == pnpm ]]; then
  command -v pnpm >/dev/null 2>&1 || {
    echo "required command is not installed: pnpm" >&2
    exit 1
  }
fi
pushd "$repo_root/client" >/dev/null
run_bounded "install official BYOP workspace dependencies" \
  "${INSTANT_BYOP_E2E_INSTALL_TIMEOUT:-10m}" "${pnpm_cmd[@]}" install --frozen-lockfile
run_bounded "install Playwright Chromium for BYOP" \
  "${INSTANT_BYOP_E2E_PLAYWRIGHT_TIMEOUT:-10m}" \
  "${pnpm_cmd[@]}" --filter @instantdb/core exec playwright install chromium
run_bounded "build official BYOP client packages" \
  "${INSTANT_BYOP_E2E_BUILD_PACKAGES_TIMEOUT:-15m}" "${pnpm_cmd[@]}" run build-packages
INSTANT_ADMIN_LIVE_E2E=0 \
INSTANT_BYOP_LIVE_E2E=1 \
INSTANT_API_URI=http://localhost:8888 \
INSTANT_BYOP_APP_ID="$app_id" \
INSTANT_BYOP_ADMIN_TOKEN="$admin_token" \
INSTANT_BYOP_COMPOSE_FILE="$compose_file" \
INSTANT_BYOP_SCHEMA="$schema" \
  "${pnpm_cmd[@]}" --filter @instantdb/admin test:e2e -- --run

mutator_token=$(node -e 'process.stdout.write(require("node:crypto").randomBytes(32).toString("hex"))')
INSTANT_BYOP_COMPOSE_FILE="$compose_file" \
INSTANT_BYOP_SCHEMA="$schema" \
INSTANT_BYOP_SLOT_NAME="$slot_name" \
INSTANT_BYOP_MUTATOR_TOKEN="$mutator_token" \
  node "$repo_root/self-hosting/byop-e2e-mutator.mjs" &
mutator_pid=$!
for _ in $(seq 1 30); do
  if curl --silent --show-error --fail --max-time 1 \
      http://127.0.0.1:8899/health >/dev/null; then
    break
  fi
  sleep 1
done
if ! curl --silent --show-error --fail --max-time 1 \
    http://127.0.0.1:8899/health >/dev/null; then
  echo 'BYOP mutation helper did not become ready within 30 seconds' >&2
  exit 1
fi

INSTANT_E2E_API_URL=http://localhost:8888 \
INSTANT_BYOP_APP_ID="$app_id" \
INSTANT_BYOP_MUTATOR_URL=http://127.0.0.1:8899/insert-visible-todo \
INSTANT_BYOP_MUTATOR_TOKEN="$mutator_token" \
  "${pnpm_cmd[@]}" --filter @instantdb/core exec vitest run --project e2e \
    __tests__/src/byop.e2e.test.ts
popd >/dev/null
