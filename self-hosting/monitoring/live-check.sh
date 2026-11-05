#!/usr/bin/env bash
# Read-only go-live check for the monitoring overlay (P7.7).
#
# Run from self-hosting/ on the host after the overlay and the canary profile are
# up. The script only reads: it asks Docker which services run, queries
# Prometheus for the series the alert rules depend on, and reads the archiver
# counters from PostgreSQL. It never prints a secret or a credential file.
#
#   bash monitoring/live-check.sh
#
# Arguments after the script name are passed to `docker compose`, so the
# deployment kit's files work too:
#
#   bash monitoring/live-check.sh --project-name backendrelay \
#     --project-directory . --env-file deploy/.env \
#     -f deploy/docker-compose.yml -f docker-compose.monitoring.yml
#
# Without arguments the Caddy compose file and the overlay in this directory are
# used, the same pair as the `dc` helper in docs/operations.md.
set -euo pipefail

export COMPOSE_FILE=${COMPOSE_FILE:-docker-compose.with-caddy.yml:docker-compose.monitoring.yml}
prom_url=${PROMETHEUS_URL:-http://127.0.0.1:9090}
database=${POSTGRES_DB:-instant}
database_user=${POSTGRES_USER:-postgres}

compose=(docker compose)
[ $# -eq 0 ] || compose+=("$@")
compose+=(--profile canary)
dc() { "${compose[@]}" "$@"; }
promql() { dc exec -T prometheus promtool query instant "$prom_url" "$1"; }
sql_value() { dc exec -T -u postgres postgres psql -U "$database_user" -d "$database" -Atc "$1"; }

failures=0
ok() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*" >&2; failures=$((failures + 1)); }

# A required series must exist. A metric, target or alert state that the rules
# read is only live while Prometheus returns at least one sample for it.
require_series() {
  local description=$1 expression=$2 output
  if ! output=$(promql "$expression" 2>&1); then
    fail "$description: Prometheus query failed: $expression"
  elif [[ $output == *'empty query result'* ]]; then
    fail "$description: no series for $expression"
  else
    ok "$description"
  fi
}

# An expression that must return nothing, for example an unhealthy target or an
# alert that has to be quiet at go-live.
require_absent() {
  local description=$1 expression=$2 output
  if ! output=$(promql "$expression" 2>&1); then
    fail "$description: Prometheus query failed: $expression"
  elif [[ $output == *'empty query result'* ]]; then
    ok "$description"
  else
    fail "$description"
    printf '%s\n' "$output" | sed 's/^/     /' >&2
  fi
}

running=$(dc ps --status running --services)
for service in prometheus alertmanager grafana node-exporter postgres-exporter \
               loki alloy docker-socket-proxy canary; do
  if grep -qx -- "$service" <<<"$running"; then
    ok "$service is running"
  else
    fail "$service is not running"
  fi
done

for job in backendrelay host postgres prometheus alertmanager canary; do
  require_series "Prometheus scrapes $job" "up{job=\"$job\"} == 1"
done
require_series 'postgres-exporter reaches the database' 'pg_up == 1'

require_series 'Watchdog is firing into the dead-man receiver' \
  'ALERTS{alertname="Watchdog",alertstate="firing"} == 1'
require_absent 'WalArchiveFailing is quiet' \
  'ALERTS{alertname="WalArchiveFailing",alertstate="firing"}'

# The rule reads these three built-in postgres-exporter series. A pinned exporter
# without the archiver collector leaves the rule silently dead.
for metric in pg_stat_archiver_archived_count \
              pg_stat_archiver_failed_count \
              pg_stat_archiver_last_archive_age; do
  require_series "postgres-exporter exposes $metric" "$metric >= 0"
done
require_absent 'no WAL archive push failed in the last 15 minutes' \
  'increase(pg_stat_archiver_failed_count[15m]) > 0'

archived=$(sql_value 'SELECT archived_count FROM pg_stat_archiver' 2>&1) || true
failed=$(sql_value 'SELECT failed_count FROM pg_stat_archiver' 2>&1) || true
case $archived in
  '' | *[!0-9]*) fail "cannot read pg_stat_archiver.archived_count: $archived" ;;
  *)
    if [ "$archived" -gt 0 ]; then
      ok "WAL segments are archived ($archived so far)"
    else
      fail 'no WAL segment has been archived yet; enable archive_mode and take a full backup first'
    fi
    ;;
esac
case $failed in
  '' | *[!0-9]*) fail "cannot read pg_stat_archiver.failed_count: $failed" ;;
  0) ok 'pg_stat_archiver.failed_count is 0' ;;
  *) fail "pg_stat_archiver.failed_count is $failed" ;;
esac

require_series 'the off-host backup timestamp is exported' \
  'backendrelay_backup_offhost_completed_seconds > 0'
require_series 'the backup snapshot timestamp is exported' \
  'backendrelay_backup_snapshot_seconds > 0'

require_series 'the canary completed a probe in the last five minutes' \
  'time() - backendrelay_canary_last_success_timestamp_seconds < 300'
require_series 'the canary records propagation samples' \
  'backendrelay_canary_propagation_seconds_count > 0'

if [ "$failures" -ne 0 ]; then
  printf '\n%d check(s) failed. See "Monitoring go-live and alert tests" in docs/operations.md.\n' \
    "$failures" >&2
  exit 1
fi
printf '\nAll wiring checks passed. Continue with the alert, dead-man and WAL tests in docs/operations.md.\n'
