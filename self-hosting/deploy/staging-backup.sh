#!/usr/bin/env bash
# Pre-deploy backup for the staging project (P7.5).
#
# deploy.sh runs this through DEPLOY_BACKUP_COMMAND, so a staging rehearsal
# exercises the same order as production: verify the images, take a backup,
# migrate, restart and smoke-test. It dumps the staging database only: it
# refuses any stack other than staging, never reads the production project and
# keeps the newest few dumps on the host. Staging data is disposable, so this is
# a local recovery point for a rehearsal, not an off-host backup.
#
# Usage: staging-backup.sh [backup-directory]
# Environment: DEPLOY_ENV_FILE (default deploy/.env.staging), STAGING_BACKUP_DIR,
# STAGING_BACKUP_RETAIN (default 5).
set -Eeuo pipefail
umask 077

deploy_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
self_hosting=$(cd -- "$deploy_dir/.." && pwd)
# shellcheck source=lib.sh
. "$deploy_dir/lib.sh"

env_file=${DEPLOY_ENV_FILE:-$deploy_dir/.env.staging}
backup_dir=${1:-${STAGING_BACKUP_DIR:-/var/lib/backendrelay-staging/backups}}
retain=${STAGING_BACKUP_RETAIN:-5}

[ -f "$env_file" ] || die "$env_file is missing; copy .env.staging.example and fill it in"
require_env_file_permissions "$env_file"
command -v docker >/dev/null 2>&1 || die "docker is required"
docker compose version >/dev/null || die "docker compose v2 is required"

project=$(env_or_file DEPLOY_COMPOSE_PROJECT)
project=${project:-backendrelay-staging}
state_dir=$(env_or_file DEPLOY_STATE_DIR)
state_dir=${state_dir:-/var/lib/backendrelay-staging/deploy}
configure_stack "$project" "$state_dir"
[ "$DEPLOY_STACK_NAME" = staging ] ||
  die "staging-backup.sh only dumps the staging database; DEPLOY_STACK is $DEPLOY_STACK_NAME"

compose=(docker compose --project-name "$project" --project-directory "$self_hosting"
  --env-file "$env_file" "${DEPLOY_COMPOSE_PROFILE_FLAGS[@]}" -f "$deploy_dir/docker-compose.yml")

install -d -m 700 "$backup_dir"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
target="$backup_dir/staging-$stamp.sql.gz"
temporary=$(mktemp "$backup_dir/.staging-$stamp.XXXXXX")
trap 'rm -f "${temporary:-}"' EXIT

# deploy.sh has just started aux-postgres; on a cold start it can still be
# replaying WAL, so wait for it to accept connections before dumping.
deadline=$((SECONDS + 60))
until "${compose[@]}" exec -T "$DEPLOY_DB_SERVICE" pg_isready --username postgres >/dev/null 2>&1; do
  if (( SECONDS >= deadline )); then
    die "$DEPLOY_DB_SERVICE is not ready; no staging backup was written"
  fi
  sleep 1
done

# pg_dumpall runs inside aux-postgres, so no credential leaves the container and
# roles, grants and the migration checksum table are included in the dump.
log "dumping $DEPLOY_DB_SERVICE to $target"
"${compose[@]}" exec -T "$DEPLOY_DB_SERVICE" pg_dumpall --username postgres |
  gzip -9 >"$temporary" || die "pg_dumpall failed; no staging backup was written"
gzip -t "$temporary" || die "the staging dump is not valid gzip"
mv -f "$temporary" "$target"
temporary=
trap - EXIT
chmod 600 "$target"
log "wrote $target ($(stat -c %s "$target") bytes)"

# Keep the newest dumps; the rehearsal only needs the last few.
while IFS= read -r old; do
  rm -f -- "$old"
  log "removed old staging dump $old"
done < <(ls -1t "$backup_dir"/staging-*.sql.gz 2>/dev/null | tail -n +$((retain + 1)))
