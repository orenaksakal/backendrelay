#!/usr/bin/env bash
# Fire WalArchiveFailing on purpose, without touching PostgreSQL or pgBackRest.
#
# node-exporter's textfile collector publishes a rising
# pg_stat_archiver_failed_count, which is exactly the series the rule reads. The
# script waits for the alert to fire in Prometheus, confirms Alertmanager has it,
# holds it long enough to hear the phone, and then removes the file so the alert
# resolves. The real archiver keeps running untouched.
#
#   sudo bash monitoring/fire-wal-alert.sh
#
# Use it on production only after live-check.sh reports WalArchiveFailing quiet.
# MONITORING_TEXTFILE_DIR must match the node-exporter mount (default
# /var/lib/node-exporter/textfile). WAIT_SECONDS bounds the wait for the rule
# (default 600; the rule has for: 5m) and HOLD_SECONDS keeps it firing (default
# 300). Arguments after the script name go to `docker compose`, as in
# live-check.sh.
set -euo pipefail

export COMPOSE_FILE=${COMPOSE_FILE:-docker-compose.with-caddy.yml:docker-compose.monitoring.yml}
textfile_dir=${MONITORING_TEXTFILE_DIR:-/var/lib/node-exporter/textfile}
prom_url=${PROMETHEUS_URL:-http://127.0.0.1:9090}
wait_seconds=${WAIT_SECONDS:-600}
hold_seconds=${HOLD_SECONDS:-300}

compose=(docker compose)
[ $# -eq 0 ] || compose+=("$@")
compose+=(--profile canary)
dc() { "${compose[@]}" "$@"; }
alert_query='ALERTS{alertname="WalArchiveFailing",alertstate="firing"} == 1'

if [ ! -d "$textfile_dir" ]; then
  echo "textfile directory $textfile_dir does not exist; set MONITORING_TEXTFILE_DIR" >&2
  exit 1
fi
if [ ! -w "$textfile_dir" ]; then
  echo "$textfile_dir is not writable; run with sudo" >&2
  exit 1
fi

series=$textfile_dir/p77-wal-alert.prom
write_series() {
  printf '# P7.7 WAL archive alert test; remove this file to resolve the alert\npg_stat_archiver_failed_count %d\n' "$1" > "$series"
}
cleanup() {
  rm -f "$series"
  echo "removed $series; WalArchiveFailing resolves after the next scrape"
}
trap cleanup EXIT
if [ -e "$series" ]; then
  echo "removing a leftover $series from an interrupted run"
  rm -f "$series"
fi

if ! idle=$(dc exec -T prometheus promtool query instant "$prom_url" "$alert_query" 2>&1); then
  echo "cannot query Prometheus: $idle" >&2
  exit 1
fi
if [[ $idle != *'empty query result'* ]]; then
  echo 'WalArchiveFailing is already firing; fix the real archive failure first' >&2
  exit 1
fi

echo "writing a rising test counter to $series"
count=0
fired=0
deadline=$((SECONDS + wait_seconds))
while [ "$SECONDS" -lt "$deadline" ]; do
  count=$((count + 1))
  write_series "$count"
  if output=$(dc exec -T prometheus promtool query instant "$prom_url" "$alert_query" 2>&1) &&
     [[ $output != *'empty query result'* ]]; then
    fired=1
    break
  fi
  sleep 15
done
if [ "$fired" -ne 1 ]; then
  echo "WalArchiveFailing did not fire within ${wait_seconds}s" >&2
  exit 1
fi
echo 'WalArchiveFailing is firing in Prometheus'

if received=$(dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 \
     alert query alertname=WalArchiveFailing 2>&1) &&
   [[ $received == *WalArchiveFailing* ]]; then
  echo 'Alertmanager has the alert; confirm the critical notification on the phone'
else
  echo 'Alertmanager does not list the alert; check its route and receiver' >&2
  printf '%s\n' "$received" | sed 's/^/     /' >&2
  exit 1
fi

echo "holding the alert for ${hold_seconds}s so the phone can be checked"
hold_deadline=$((SECONDS + hold_seconds))
while [ "$SECONDS" -lt "$hold_deadline" ]; do
  count=$((count + 1))
  write_series "$count"
  sleep 15
done
