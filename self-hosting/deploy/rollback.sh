#!/usr/bin/env bash
# Roll the BackendRelay stack back to the images recorded by the last
# successful deploy (or to the two image references given on the command line).
#
# Usage: rollback.sh [--mode manual|deploy-failure] [server-image dashboard-image]
#
# The target image is first tested with `migrate up`. When it refuses a newer
# non-additive schema, the migrations that the failed candidate applied are
# rolled back with the candidate image, then the target is started. See
# README.md and the schema policy in docs/development.md.
set -Eeuo pipefail
umask 077

deploy_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
self_hosting=$(cd -- "$deploy_dir/.." && pwd)
# shellcheck source=lib.sh
. "$deploy_dir/lib.sh"

env_file=${DEPLOY_ENV_FILE:-$deploy_dir/.env}
# DEPLOY_ENV_FILE selects the file; everything else may live in it or in the
# environment. Shell values win.
state_dir=$(env_or_file DEPLOY_STATE_DIR)
state_dir=${state_dir:-/var/lib/backendrelay/deploy}
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
smoke_api_url=$(env_or_file DEPLOY_SMOKE_API_URL)
migrate_down=$(env_or_file ROLLBACK_MIGRATE_DOWN)
migrate_down=${migrate_down:-auto}
export ROLLBACK_MIGRATE_DOWN=$migrate_down
mode=manual

usage() {
  cat >&2 <<'EOF'
usage: rollback.sh [--mode manual|deploy-failure] [server-image dashboard-image]

With no images, uses $DEPLOY_STATE_DIR/previous.env from the last deploy.
Environment: DEPLOY_STATE_DIR, DEPLOY_ENV_FILE, DEPLOY_STACK, DEPLOY_MONITORING,
DEPLOY_SERVER_SERVICES, DEPLOY_SMOKE_API_URL, DEPLOY_EXPECT_CLOUDFLARE,
ROLLBACK_MIGRATE_DOWN=auto|never.
EOF
  exit 2
}

if [ "${1:-}" = --mode ]; then
  [ "$#" -ge 2 ] || usage
  mode=$2
  shift 2
fi
case "$mode" in
  manual | deploy-failure) ;;
  *) usage ;;
esac
[ "$#" -eq 0 ] || [ "$#" -eq 2 ] || usage

[ -f "$env_file" ] || die "$env_file is missing"
require_env_file_permissions "$env_file"
command -v docker >/dev/null 2>&1 || die "docker is required"
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"
docker compose version >/dev/null || die "docker compose v2 is required"

# A staging rollback must use the staging project, network and state; the same
# helper deploy.sh uses refuses a mixed configuration.
configure_stack "$project" "$state_dir"

compose_files=(-f "$deploy_dir/docker-compose.yml")
if [ "$monitoring" = 1 ]; then
  compose_files+=(-f "$self_hosting/docker-compose.monitoring.yml")
fi
compose=(docker compose --project-name "$project" --project-directory "$self_hosting"
  --env-file "$env_file" "${DEPLOY_COMPOSE_PROFILE_FLAGS[@]}" "${compose_files[@]}")

current_state="$state_dir/current.env"
previous_state="$state_dir/previous.env"
candidate_state="$state_dir/candidate.env"

if [ "$#" -eq 2 ]; then
  target_server=$1
  target_dashboard=$2
else
  target_server=$(state_value "$previous_state" INSTANT_SERVER_IMAGE)
  target_dashboard=$(state_value "$previous_state" INSTANT_DASHBOARD_IMAGE)
fi
[ -n "$target_server" ] && [ -n "$target_dashboard" ] ||
  die "no rollback images recorded in $previous_state; pass them explicitly"

current_server=${DEPLOY_ROLLBACK_CURRENT_SERVER:-$(state_value "$candidate_state" INSTANT_SERVER_IMAGE)}
current_server=${current_server:-$(state_value "$current_state" INSTANT_SERVER_IMAGE)}
[ -n "$current_server" ] ||
  die "cannot determine the image that applied the current schema; pass the candidate image in DEPLOY_ROLLBACK_CURRENT_SERVER"

"$self_hosting/verify-release-images.sh" "$target_server" "$target_dashboard"

log "rolling back to $target_server and $target_dashboard"
if [ "$mode" = deploy-failure ]; then
  log "deploy-failure mode: rolling back without another backup; the failed deploy took one first"
else
  warn "no new backup is taken by rollback.sh; confirm the pre-deploy backup is usable"
fi

migrate_to "$target_server" "$current_server"

read -r -a server_service_list <<<"$server_services"
[ "${#server_service_list[@]}" -gt 0 ] || die "DEPLOY_SERVER_SERVICES is empty"
for service in "${server_service_list[@]}"; do
  log "restarting $service with $target_server"
  INSTANT_SERVER_IMAGE="$target_server" "${compose[@]}" up -d --no-deps --force-recreate "$service"
  wait_for_server "$service" "$health_timeout" ||
    die "$service did not become healthy within ${health_timeout}s"
done

log "restarting dashboard with $target_dashboard"
INSTANT_DASHBOARD_IMAGE="$target_dashboard" "${compose[@]}" up -d --no-deps --force-recreate dashboard
wait_for_dashboard "$health_timeout" || die "dashboard did not become healthy within ${health_timeout}s"

"${compose[@]}" up -d --no-deps caddy

api_url=${smoke_api_url:-https://$(env_value "$env_file" BACKEND_DOMAIN)}
[ -n "$api_url" ] || die "set DEPLOY_SMOKE_API_URL or BACKEND_DOMAIN"
api_url=${api_url%/}
health=$(wait_for_public_health "$api_url" "$smoke_timeout") ||
  die "public /health/system is not reachable after rollback"
printf '%s' "$health" | grep -q '"wal":"ok"' ||
  die "public /health/system did not report {\"wal\":\"ok\"} after rollback: $health"

check_client_ip "$api_url" "$smoke_timeout" "$expect_cloudflare"

write_state "$current_state" "$target_server" "$target_dashboard"
log "rolled back to $target_server and $target_dashboard"
