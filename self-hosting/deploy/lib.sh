# Shared helpers for deploy.sh and rollback.sh. Source this file; do not run it.
# The caller owns `set -Eeuo pipefail` and the `compose` array.

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2
}

warn() {
  log "WARNING: $*"
}

die() {
  log "ERROR: $*"
  exit 1
}

# Reads one unquoted or simply quoted value from a Compose env file. It never
# sources the file, so a malformed env file cannot execute code.
env_value() {
  local file=$1 key=$2
  [ -r "$file" ] || return 0
  sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*//p" "$file" | tail -n 1 |
    sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'$//"
}

# Reads a value from the environment first, then from the Compose env file.
# Uses the caller's `env_file` variable.
env_or_file() {
  local name=$1 value
  value=$(printenv "$name" 2>/dev/null || true)
  [ -n "$value" ] || value=$(env_value "$env_file" "$name")
  printf '%s' "$value"
}

# The one-off migration container needs an address that differs from the
# serving server's static address. Read DEPLOY_MIGRATE_IP, or default per stack.
migration_address() {
  local value
  value=$(env_or_file DEPLOY_MIGRATE_IP)
  if [ -z "$value" ]; then
    case "${DEPLOY_STACK_NAME:-production}" in
      staging) value=172.31.0.250 ;;
      *) value=172.30.0.250 ;;
    esac
  fi
  printf '%s' "$value"
}

# Resolves DEPLOY_STACK for the shared compose file. Sets, for the caller:
#   DEPLOY_STACK_NAME              production or staging
#   DEPLOY_COMPOSE_PROFILE_FLAGS   compose flags that enable the stack's profile
#   DEPLOY_DB_SERVICE              postgres or aux-postgres (exported for Compose)
# and refuses a staging configuration that would reach into the production
# project, network or rollback state. Arguments: compose project, state dir.
configure_stack() {
  local project=$1 state=$2 network declared
  DEPLOY_STACK_NAME=$(env_or_file DEPLOY_STACK)
  DEPLOY_STACK_NAME=${DEPLOY_STACK_NAME:-production}
  case "$DEPLOY_STACK_NAME" in
    production)
      DEPLOY_COMPOSE_PROFILE_FLAGS=()
      DEPLOY_DB_SERVICE=postgres
      ;;
    staging)
      # aux-postgres is the staging database. Its profile keeps the staging
      # passwords out of a production deploy.
      DEPLOY_COMPOSE_PROFILE_FLAGS=(--profile staging)
      DEPLOY_DB_SERVICE=aux-postgres
      if [ "$project" = backendrelay ]; then
        die "DEPLOY_STACK=staging needs its own DEPLOY_COMPOSE_PROJECT; backendrelay is the production project"
      fi
      network=$(env_or_file DEPLOY_NETWORK_NAME)
      if [ -z "$network" ] || [ "$network" = backendrelay ]; then
        die "DEPLOY_STACK=staging needs its own DEPLOY_NETWORK_NAME; backendrelay is the production network"
      fi
      if [ "$state" = /var/lib/backendrelay/deploy ]; then
        die "DEPLOY_STACK=staging needs its own DEPLOY_STATE_DIR; /var/lib/backendrelay/deploy holds the production rollback state"
      fi
      ;;
    *)
      die "DEPLOY_STACK must be production or staging, not $DEPLOY_STACK_NAME"
      ;;
  esac
  # Compose builds DATABASE_URL from DEPLOY_DB_SERVICE. Reject a file or shell
  # value that disagrees with the stack instead of silently pointing at the
  # other database.
  declared=$(env_or_file DEPLOY_DB_SERVICE)
  if [ -n "$declared" ] && [ "$declared" != "$DEPLOY_DB_SERVICE" ]; then
    die "DEPLOY_DB_SERVICE=$declared does not match DEPLOY_STACK=$DEPLOY_STACK_NAME, which uses $DEPLOY_DB_SERVICE"
  fi
  # Child scripts (rollback.sh, the pre-deploy backup command) resolve the same
  # stack and database.
  export DEPLOY_STACK="$DEPLOY_STACK_NAME"
  export DEPLOY_DB_SERVICE
}

# Reads a value from a state file written by write_state.
state_value() {
  local file=$1 key=$2
  [ -r "$file" ] || return 0
  sed -n "s/^${key}=//p" "$file" | tail -n 1
}

# Writes the image state atomically and mode 0600. The state directory holds no
# secrets, but it must not be editable by other local accounts.
write_state() {
  local file=$1 server=$2 dashboard=$3 temporary
  temporary=$(mktemp "${file}.XXXXXX")
  {
    printf '# Written by the BackendRelay deploy kit at %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'INSTANT_SERVER_IMAGE=%s\n' "$server"
    printf 'INSTANT_DASHBOARD_IMAGE=%s\n' "$dashboard"
  } >"$temporary"
  chmod 600 "$temporary"
  mv -f "$temporary" "$file"
}

require_env_file_permissions() {
  local file=$1 permissions
  permissions=$(stat -c %a "$file")
  if (( (8#$permissions & 8#77) != 0 )); then
    die "$file must not be readable or writable by group or other (chmod 600)"
  fi
}

# Waits for the server container's own health check, so the probe goes through
# the same WAL readiness path as Docker's health check.
wait_for_server() {
  local service=$1 timeout=$2 deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do
    if "${compose[@]}" exec -T "$service" /usr/local/bin/instant-server healthcheck >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  log "$service did not become healthy within ${timeout}s"
  return 1
}

# Waits for the dashboard image's /healthz endpoint.
wait_for_dashboard() {
  local timeout=$1 deadline=$((SECONDS + $1))
  while (( SECONDS < deadline )); do
    if "${compose[@]}" exec -T dashboard wget -qO- http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  log "dashboard did not become healthy within ${timeout}s"
  return 1
}

# Waits for the public /health/system endpoint through Caddy.
wait_for_public_health() {
  local api_url=$1 timeout=$2 deadline=$((SECONDS + 30)) body
  while (( SECONDS < deadline )); do
    if body=$(curl --fail --silent --show-error --max-time "$timeout" "$api_url/health/system"); then
      printf '%s' "$body"
      return 0
    fi
    sleep 1
  done
  return 1
}

# Fails when the public edge cannot reach the server or when Caddy hands the
# server a private address instead of the Cloudflare client address.
#
# Caddy logs both `client_ip` (resolved from CF-Connecting-IP) and `remote_ip`
# (the Cloudflare edge). With Cloudflare in front they must differ and the
# resolved client must be a public address. The server's own view is checked by
# its unit tests for client_ip.rs; this probe proves the edge resolved it.
check_client_ip() {
  local api_url=$1 timeout=$2 expect_cloudflare=$3 marker line resolved client remote deadline
  marker="deploy-smoke-${RANDOM}-${SECONDS}"
  curl --fail --silent --show-error --max-time "$timeout" \
    "$api_url/health/system?deploy_smoke=$marker" >/dev/null ||
    die "could not reach $api_url/health/system"

  deadline=$((SECONDS + 10))
  line=
  while (( SECONDS < deadline )); do
    line=$("${compose[@]}" logs --since 2m --no-log-prefix caddy 2>/dev/null |
      grep -F -- "$marker" | tail -n 1 || true)
    [ -n "$line" ] && break
    sleep 1
  done
  [ -n "$line" ] || die "Caddy did not log the smoke request; enable its access log"

  if [ "$expect_cloudflare" = local ]; then
    log "client-ip smoke is in local mode; skipping the public-address assertions"
    return 0
  fi

  resolved=$(printf '%s\n' "$line" | python3 -c '
import json
import sys

try:
    entry = json.loads(sys.stdin.readline())
except Exception:
    sys.exit(2)
request = entry.get("request", {})
client = request.get("client_ip")
remote = request.get("remote_ip")
if not client:
    sys.exit(3)
print(client, remote or "")
') || die "could not read client_ip from the Caddy access log"

  read -r client remote <<<"$resolved"
  printf '%s' "$client" | python3 -c '
import ipaddress
import sys

try:
    address = ipaddress.ip_address(sys.stdin.read().strip())
except ValueError:
    sys.exit(2)
sys.exit(0 if address.is_global else 1)
' || die "Caddy resolved a non-public client address ($client); check trusted_proxies and client_ip_headers"

  if [ "$expect_cloudflare" = 1 ] && [ "$client" = "$remote" ]; then
    die "Caddy used the edge address ($client) instead of CF-Connecting-IP; check trusted_proxies"
  fi

  if "${compose[@]}" logs --since 2m --no-log-prefix server 2>/dev/null |
    grep -Fq 'per-IP magic-code quota is inactive'; then
    die "the server sees a private client address; check INSTANT_TRUSTED_PROXY_CIDRS and Caddy's X-Forwarded-For"
  fi

  log "client-ip smoke passed: public client $client, proxy peer ${remote:-unknown}"
}

# Runs migrate up with a target server image. When the target binary refuses a
# newer non-additive schema, rolls the extra migrations back with the image
# that applied them and retries. Prints the database/binary versions involved.
migrate_to() {
  local target_server=$1 current_server=$2 output database binary steps migrate_ip
  migrate_ip=$(migration_address)
  if output=$(SERVER_IP="$migrate_ip" INSTANT_SERVER_IMAGE="$target_server" "${compose[@]}" run --rm --no-deps -T server migrate up 2>&1); then
    printf '%s\n' "$output" >&2
    return 0
  fi

  if [[ $output =~ database\ schema\ version\ ([0-9]+)\ is\ newer\ than\ this\ binary.*version\ ([0-9]+) ]]; then
    database=${BASH_REMATCH[1]}
    binary=${BASH_REMATCH[2]}
    steps=$((database - binary))
    [ "$steps" -gt 0 ] || die "the target image reported a newer schema without a positive step count"
    if [ "${ROLLBACK_MIGRATE_DOWN:-auto}" != auto ]; then
      printf '%s\n' "$output" >&2
      die "the target image needs $steps migration(s) rolled back but ROLLBACK_MIGRATE_DOWN is not auto"
    fi
    warn "rolling back $steps migration(s) with $current_server (database $database, target binary $binary)"
    warn "migrate down runs the .down.sql files and can discard data; restore the pre-deploy backup instead when that data matters"
    SERVER_IP="$migrate_ip" INSTANT_SERVER_IMAGE="$current_server" "${compose[@]}" run --rm --no-deps -T server migrate down "$steps" ||
      die "could not roll back $steps migration(s) with $current_server"
    SERVER_IP="$migrate_ip" INSTANT_SERVER_IMAGE="$target_server" "${compose[@]}" run --rm --no-deps -T server migrate up ||
      die "the target image still cannot migrate after rolling back $steps step(s)"
    return 0
  fi

  printf '%s\n' "$output" >&2
  die "the target server image cannot run against the current schema"
}
