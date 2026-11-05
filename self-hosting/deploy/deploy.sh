#!/usr/bin/env bash
# Deploy one signed server and dashboard image to the BackendRelay stack.
#
# Usage: deploy.sh <server-image@sha256:...> <dashboard-image@sha256:...>
#
# The script verifies signatures, takes a verified backup, migrates with the new
# server image, restarts one server replica at a time, restarts the dashboard,
# and runs the public smoke tests. Any failure after the first mutation rolls
# back to the images recorded by the previous successful deploy. See README.md.
set -Eeuo pipefail
umask 077

deploy_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
self_hosting=$(cd -- "$deploy_dir/.." && pwd)
# shellcheck source=lib.sh
. "$deploy_dir/lib.sh"

env_file=${DEPLOY_ENV_FILE:-$deploy_dir/.env}
# DEPLOY_ENV_FILE selects the file; everything else may live in it or in the
# environment. Shell values win. Child scripts and the pre-deploy backup
# command inherit the resolved path.
export DEPLOY_ENV_FILE="$env_file"
state_dir=$(env_or_file DEPLOY_STATE_DIR)
state_dir=${state_dir:-/var/lib/backendrelay/deploy}
recovery_config=$(env_or_file DEPLOY_RECOVERY_CONFIG)
recovery_config=${recovery_config:-/etc/backendrelay/backup.json}
health_timeout=$(env_or_file DEPLOY_HEALTH_TIMEOUT_SECONDS)
health_timeout=${health_timeout:-60}
smoke_timeout=$(env_or_file DEPLOY_SMOKE_TIMEOUT_SECONDS)
smoke_timeout=${smoke_timeout:-30}
expect_cloudflare=$(env_or_file DEPLOY_EXPECT_CLOUDFLARE)
expect_cloudflare=${expect_cloudflare:-1}
monitoring=$(env_or_file DEPLOY_MONITORING)
monitoring=${monitoring:-0}
project=$(env_or_file DEPLOY_COMPOSE_PROJECT)
project=${project:-backendrelay}
server_services=$(env_or_file DEPLOY_SERVER_SERVICES)
server_services=${server_services:-server}
first_deploy=$(env_or_file DEPLOY_FIRST_DEPLOY)
first_deploy=${first_deploy:-0}
skip_canary_smoke=$(env_or_file DEPLOY_SKIP_CANARY_SMOKE)
skip_canary_smoke=${skip_canary_smoke:-0}
backup_command=$(env_or_file DEPLOY_BACKUP_COMMAND)
smoke_api_url=$(env_or_file DEPLOY_SMOKE_API_URL)
migrate_down=$(env_or_file ROLLBACK_MIGRATE_DOWN)
migrate_down=${migrate_down:-auto}
mutated=0
rolling_back=0

usage() {
  cat >&2 <<'EOF'
usage: deploy.sh <server-image@sha256:...> <dashboard-image@sha256:...>

Environment:
  DEPLOY_ENV_FILE                 Compose env file (default deploy/.env)
  DEPLOY_STACK                    production (default) or staging; selects the
                                  database service and the compose profile
  DEPLOY_STATE_DIR                Previous/current image state (default /var/lib/backendrelay/deploy)
  DEPLOY_RECOVERY_CONFIG          recovery.py config (default /etc/backendrelay/backup.json)
  DEPLOY_FIRST_DEPLOY             1 only on an empty database; skips the backup
  DEPLOY_BACKUP_COMMAND           Alternative backup command for a managed database
  DEPLOY_MONITORING               1 to merge docker-compose.monitoring.yml
  DEPLOY_SERVER_SERVICES          Space-separated server services (default "server")
  DEPLOY_MIGRATE_IP               One-off migration container address (default
                                  172.30.0.250, or 172.31.0.250 for staging)
  DEPLOY_SMOKE_API_URL            Public API origin for the smoke test
  DEPLOY_EXPECT_CLOUDFLARE        1 when the edge is behind Cloudflare (default 1)
  DEPLOY_SKIP_CANARY_SMOKE        1 only for the first deploy, before the canary app exists
EOF
  exit 2
}

[ "$#" -eq 2 ] || usage
server_image=$1
dashboard_image=$2

[ -f "$env_file" ] || die "$env_file is missing; copy .env.example and fill it in"
require_env_file_permissions "$env_file"
command -v docker >/dev/null 2>&1 || die "docker is required"
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"
command -v flock >/dev/null 2>&1 || die "flock is required (util-linux)"
docker compose version >/dev/null || die "docker compose v2 is required"

# Resolve the stack before touching anything: DEPLOY_DB_SERVICE and the
# profile flags come from DEPLOY_STACK, and a staging file that reuses the
# production project, network or state directory is refused here.
configure_stack "$project" "$state_dir"

# The migrate one-off must not try to take the running server's static address.
# Use a dedicated address in the same subnet unless the operator overrides it.
migrate_ip=$(migration_address)

# Fail before touching anything when either image is not a canonical digest or
# does not carry a trusted signature.
"$self_hosting/verify-release-images.sh" "$server_image" "$dashboard_image"

compose_files=(-f "$deploy_dir/docker-compose.yml")
if [ "$monitoring" = 1 ]; then
  compose_files+=(-f "$self_hosting/docker-compose.monitoring.yml")
fi
compose=(docker compose --project-name "$project" --project-directory "$self_hosting"
  --env-file "$env_file" "${DEPLOY_COMPOSE_PROFILE_FLAGS[@]}" "${compose_files[@]}")

install -d -m 700 "$state_dir"
exec 9>"$state_dir/deploy.lock"
flock -n 9 || die "another deploy is already running in $state_dir"

current_state="$state_dir/current.env"
previous_state="$state_dir/previous.env"
candidate_state="$state_dir/candidate.env"

previous_server=$(state_value "$current_state" INSTANT_SERVER_IMAGE)
previous_dashboard=$(state_value "$current_state" INSTANT_DASHBOARD_IMAGE)
if [ -z "$previous_server" ] || [ -z "$previous_dashboard" ]; then
  previous_server=$(env_value "$env_file" INSTANT_SERVER_IMAGE)
  previous_dashboard=$(env_value "$env_file" INSTANT_DASHBOARD_IMAGE)
fi
# Ignore the .env.example placeholders and any tag-only fallback; they are not a
# rollback target. Deploy.sh always passes digest-only arguments.
if [ -n "$previous_server" ] && ! "$self_hosting/verify-release-images.sh" --shape-only "$previous_server" >/dev/null 2>&1; then
  previous_server=
fi
if [ -n "$previous_dashboard" ] && ! "$self_hosting/verify-release-images.sh" --shape-only "$previous_dashboard" >/dev/null 2>&1; then
  previous_dashboard=
fi

write_state "$candidate_state" "$server_image" "$dashboard_image"
if [ -n "$previous_server" ] && [ -n "$previous_dashboard" ]; then
  write_state "$previous_state" "$previous_server" "$previous_dashboard"
else
  rm -f "$previous_state"
  warn "no previous image state found; this deploy cannot roll back automatically"
fi

on_error() {
  local status=$?
  trap - ERR
  if [ "$rolling_back" = 1 ] || [ "$mutated" = 0 ]; then
    exit "$status"
  fi
  rolling_back=1
  if [ -z "$previous_server" ] || [ -z "$previous_dashboard" ]; then
    log "deployment failed before a previous image could be recorded; manual recovery required"
    exit "$status"
  fi
  log "deployment failed; rolling back to $previous_server and $previous_dashboard"
  if DEPLOY_ROLLBACK_CURRENT_SERVER="$server_image" \
    DEPLOY_ROLLBACK_CURRENT_DASHBOARD="$dashboard_image" \
    DEPLOY_STATE_DIR="$state_dir" DEPLOY_ENV_FILE="$env_file" \
    DEPLOY_STACK="$DEPLOY_STACK_NAME" \
    DEPLOY_COMPOSE_PROJECT="$project" DEPLOY_MONITORING="$monitoring" \
    DEPLOY_HEALTH_TIMEOUT_SECONDS="$health_timeout" \
    DEPLOY_SMOKE_TIMEOUT_SECONDS="$smoke_timeout" \
    DEPLOY_EXPECT_CLOUDFLARE="$expect_cloudflare" \
    DEPLOY_SERVER_SERVICES="$server_services" \
    DEPLOY_SMOKE_API_URL="$smoke_api_url" \
    ROLLBACK_MIGRATE_DOWN="$migrate_down" \
    bash "$deploy_dir/rollback.sh" --mode deploy-failure "$previous_server" "$previous_dashboard"; then
    log "rollback completed; the deployment failed and the previous images are serving"
  else
    log "AUTOMATIC ROLLBACK FAILED; restore the pre-deploy backup and investigate before changing anything"
  fi
  exit "$status"
}
trap on_error ERR

# Start this stack's database if it is stopped: `postgres` in production,
# `aux-postgres` in staging. This does not change its data and must happen
# before recovery.py or the staging dump reads the database.
"${compose[@]}" up -d "$DEPLOY_DB_SERVICE"

take_backup() {
  local config=$recovery_config
  if [ -n "$backup_command" ]; then
    bash -c "$backup_command" ||
      die "the configured pre-deploy backup command failed; aborting"
    return 0
  fi
  if [ "$first_deploy" = 1 ]; then
    warn "DEPLOY_FIRST_DEPLOY=1: skipping the pre-deploy backup on an empty database"
    return 0
  fi
  [ -r "$recovery_config" ] || die "$recovery_config is missing; no backup, no deploy"
  if [ -n "$previous_server" ] && [ -n "$previous_dashboard" ]; then
    # recovery.py refuses a candidate image that is not the running image. A
    # pre-deploy backup describes the state we are leaving, so point its
    # candidateImages at the currently deployed digests.
    config="$state_dir/backup-config.json"
    python3 - "$recovery_config" "$config" "$previous_server" "$previous_dashboard" <<'PY'
import json
import sys

source, target, server, dashboard = sys.argv[1:]
with open(source, encoding='utf-8') as handle:
    config = json.load(handle)
config['candidateImages'] = {'server': server, 'dashboard': dashboard}
with open(target, 'w', encoding='utf-8') as handle:
    json.dump(config, handle, indent=2)
    handle.write('\n')
PY
    chmod 600 "$config"
  fi
  python3 "$self_hosting/recovery.py" backup --config "$config" ||
    die "the pre-deploy backup failed; aborting before any change"
}

log "taking a pre-deploy backup through $recovery_config"
take_backup
log "pre-deploy backup completed"

mutated=1
log "applying migrations with $server_image"
SERVER_IP="$migrate_ip" INSTANT_SERVER_IMAGE="$server_image" \
  "${compose[@]}" run --rm --no-deps -T server migrate up

read -r -a server_service_list <<<"$server_services"
[ "${#server_service_list[@]}" -gt 0 ] || die "DEPLOY_SERVER_SERVICES is empty"
for service in "${server_service_list[@]}"; do
  log "restarting $service with $server_image"
  INSTANT_SERVER_IMAGE="$server_image" "${compose[@]}" up -d --no-deps --force-recreate "$service"
  wait_for_server "$service" "$health_timeout" ||
    die "$service did not become healthy within ${health_timeout}s"
  log "$service is healthy"
done

log "restarting dashboard with $dashboard_image"
INSTANT_DASHBOARD_IMAGE="$dashboard_image" "${compose[@]}" up -d --no-deps --force-recreate dashboard
wait_for_dashboard "$health_timeout" || die "dashboard did not become healthy within ${health_timeout}s"

# Recreate Caddy only when its configuration or image changed; a no-op keeps
# existing TLS connections and SSE sessions alive.
"${compose[@]}" up -d --no-deps caddy

api_url=${smoke_api_url:-https://$(env_value "$env_file" BACKEND_DOMAIN)}
[ -n "$api_url" ] || die "set DEPLOY_SMOKE_API_URL or BACKEND_DOMAIN"
api_url=${api_url%/}
log "checking $api_url/health/system"
health=$(wait_for_public_health "$api_url" "$smoke_timeout") ||
  die "public /health/system is not reachable"
printf '%s' "$health" | grep -q '"wal":"ok"' ||
  die "public /health/system did not report {\"wal\":\"ok\"}: $health"

check_client_ip "$api_url" "$smoke_timeout" "$expect_cloudflare"

if [ "$skip_canary_smoke" = 1 ]; then
  warn "skipping the canary SDK query; this is only valid before the canary app exists"
else
  canary_app=$(env_value "$env_file" CANARY_APP_ID)
  canary_token=$(env_value "$env_file" CANARY_ADMIN_TOKEN_FILE)
  [ -n "$canary_app" ] || die "CANARY_APP_ID is required for the SDK smoke test"
  [ -n "$canary_token" ] || die "CANARY_ADMIN_TOKEN_FILE is required for the SDK smoke test"
  log "running the SDK query against canary app $canary_app"
  "${compose[@]}" --profile smoke run --rm --no-deps -T \
    -e SMOKE_API_URL="$api_url" \
    -e SMOKE_APP_ID="$canary_app" \
    smoke
fi

log "running instant-server audit"
"${compose[@]}" exec -T server /usr/local/bin/instant-server audit

write_state "$current_state" "$server_image" "$dashboard_image"
log "deployed $server_image and $dashboard_image"
