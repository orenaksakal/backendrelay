#!/usr/bin/env bash
set -Eeuo pipefail

# Injects the failure modes from docs/operations.md#drills into the disposable
# stack from stack.sh while a load generator is running, then reports how long
# the stack needed to recover and whether the P4 alert rules would have fired.
#
# Usage:
#   drills.sh kill9           SIGKILL the server, then restart it
#   drills.sh walsender       terminate the invalidator's walsender four times
#   drills.sh pg-restart      restart PostgreSQL under a serving server
#   drills.sh invalidate-slot invalidate the invalidator slot while down
#   drills.sh disk-pressure   hold the server down for ten minutes with a
#                             small max_slot_wal_keep_size and fill WAL
#   drills.sh --help
#
# Recovery means: the recorded server process is alive, /health/system returns
# 200, and a SQL canary finds an active wal2json invalidator slot with a usable
# WAL position. Every drill prints ISO start/end timestamps, the seconds from
# the injected fault to that state, and the expected alert names.
#
# The stack has no supervisor, so kill9, invalidate-slot and disk-pressure
# restart the recorded binary the way a service unit's Restart=always would:
# with the environment captured from the running process. That environment is
# not written to disk or printed. The drill measures recovery from server
# failure, not a production supervisor's restart delay.
#
# invalidate-slot and disk-pressure remove the capped WAL at a checkpoint, so
# PostgreSQL marks the invalidator slot lost. Slot names carry a per-process
# machine id, so the restarted server writes a fresh slot instead of resuming
# the lost one; the report prints both so the old lost slot (dropped later by
# the hourly replication-slot cleanup) is visible. disk-pressure never fills
# the host: it proves the cap bounds WAL retention while the server is down and
# reports the resulting free space, not an artificial 100%-full filesystem.
#
# Environment overrides:
#   SCALE_STATE_DIR, SCALE_DRILL_TIMEOUT (seconds, default 180),
#   SCALE_WALSENDER_KILLS (default 3), SCALE_WALSENDER_FLAP_TIMEOUT (seconds,
#   default 720; 0 skips the flap check), SCALE_SLOT_WAL_KEEP_SIZE (default 32MB),
#   SCALE_DISK_PRESSURE_KEEP_SIZE (default 64MB),
#   SCALE_DISK_PRESSURE_SECONDS (default 600), SCALE_DISK_PRESSURE_TICK,
#   SCALE_DISK_PRESSURE_WAL_MB, SCALE_SLOT_WAL_ROUND_MB, SCALE_SLOT_WAL_ROUNDS,
#   and SCALE_PROMETHEUS_URL (default http://127.0.0.1:9090). Alerts are only
#   observed when a Prometheus is reachable there; scale-local does not run one.

scale_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# Paths and process helpers (pid_alive, data_dir, stack_json, state_dir,
# server_log, postgres_log) come from stack.sh. main() does not run because it
# is not the executed script.
source "$scale_dir/stack.sh"

readonly drill_timeout=${SCALE_DRILL_TIMEOUT:-180}
readonly walsender_kills=${SCALE_WALSENDER_KILLS:-3}
# The flap check waits for the ten-minute restart window to age out; 0 skips it.
readonly walsender_flap_timeout=${SCALE_WALSENDER_FLAP_TIMEOUT:-720}
readonly slot_wal_keep_size=${SCALE_SLOT_WAL_KEEP_SIZE:-32MB}
readonly disk_pressure_keep_size=${SCALE_DISK_PRESSURE_KEEP_SIZE:-64MB}
readonly disk_pressure_seconds=${SCALE_DISK_PRESSURE_SECONDS:-600}
readonly disk_pressure_tick=${SCALE_DISK_PRESSURE_TICK:-30}
readonly disk_pressure_wal_mb=${SCALE_DISK_PRESSURE_WAL_MB:-8}
readonly wal_round_mb=${SCALE_SLOT_WAL_ROUND_MB:-32}
readonly wal_rounds=${SCALE_SLOT_WAL_ROUNDS:-16}
readonly alert_interval=5

drill_name=
drill_started_ms=
last_restart_ms=
origin_url=
server_pid=
postmaster_pid=
server_binary=
sut_cpus=
postgres_bin=
database_url=
prometheus_url=
alerts_checked=false
alerts_seen=
alert_names=()
captured_environment=()
slot_keep_altered=false

drills_usage() {
  cat <<'EOF'
usage: drills.sh kill9 | walsender | pg-restart | invalidate-slot |
                 disk-pressure | --help

Each subcommand injects one failure into the running scale-local stack, waits
for /health/system and a SQL canary to pass again, and prints:

  drill=<name> phase=<phase> at=<iso-8601 UTC> ...

alerts are reported only when SCALE_PROMETHEUS_URL answers /api/v1/alerts;
otherwise the line says observed=unavailable. See the file header for the
environment overrides and for how a downed server is restarted without a
supervisor. Run a load generator (run.sh) first so recovery is measured under
load.
EOF
}

report() {
  printf 'drill=%s %s\n' "$drill_name" "$*"
}

fail() {
  printf 'drill=%s phase=failed at=%s reason=%s\n' \
    "$drill_name" "$(iso_now)" "$*" >&2
  exit 1
}

iso_now() {
  date -u +%Y-%m-%dT%H:%M:%SZ
}

now_ms() {
  date +%s%3N
}

elapsed_seconds() {
  local started=$1
  awk -v start="$started" -v now="$(now_ms)" \
    'BEGIN { printf "%.1f", (now - start) / 1000 }'
}

stack_value() {
  python3 - "$stack_json" "$1" <<'PY'
import json
import sys

print(json.load(open(sys.argv[1]))[sys.argv[2]])
PY
}

set_stack_pids() {
  python3 - "$stack_json" "$@" <<'PY'
import json
import sys

path, assignments = sys.argv[1], sys.argv[2:]
stack = json.load(open(path))
for assignment in assignments:
    key, _, value = assignment.partition("=")
    stack[key] = int(value)
with open(path, "w") as handle:
    json.dump(stack, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

load_stack() {
  [[ -f "$stack_json" ]] || fail "no stack recorded at $stack_json; run stack.sh up first"
  origin_url=$(stack_value origin_url)
  server_pid=$(stack_value server_pid)
  postmaster_pid=$(stack_value postmaster_pid)
  server_binary=$(stack_value server_binary)
  sut_cpus=$(stack_value sut_cpus)
  postgres_bin=$(stack_value postgres_bin)
  database_url=$(stack_value database_url)
  # The drills only ever touch the disposable loopback stack recorded by
  # stack.sh; anything else could take down a real deployment.
  [[ "$origin_url" == http://127.0.0.1:* || "$origin_url" == http://localhost:* ]] ||
    fail "refusing a non-loopback stack origin: $origin_url"
  [[ -x "$server_binary" ]] ||
    fail "recorded server binary is not executable: $server_binary"
}

sql() {
  timeout 30 "$postgres_bin/psql" "$database_url" -X --no-psqlrc \
    -A -t -v ON_ERROR_STOP=1 -c "$1"
}

health_ok() {
  curl --silent --fail --max-time 2 --output /dev/null \
    "$origin_url/health/system" 2>/dev/null
}

database_ok() {
  [[ "$(sql 'SELECT 1' 2>/dev/null)" == 1 ]]
}

# A healthy process already implies a running invalidator stream; the SQL
# canary additionally proves PostgreSQL accepts queries and that the stream
# owns an active slot with a usable WAL position instead of a lost one.
canary_ok() {
  local usable
  usable=$(sql "SELECT count(*) FROM pg_replication_slots
                WHERE active
                  AND plugin = 'wal2json'
                  AND slot_name LIKE 'invalidator\\_%' ESCAPE '\\'
                  AND wal_status <> 'lost'" 2>/dev/null) || return 1
  [[ "$usable" =~ ^[0-9]+$ ]] && ((usable > 0))
}

log_tail() {
  [[ -s "$server_log" ]] || return 0
  printf 'last server log lines:\n' >&2
  tail -n 20 "$server_log" >&2
}

# pid_alive treats a reaped-but-not-yet-waited child as alive; a drill must not
# report recovery for a server that has already exited.
server_alive() {
  local stat
  pid_alive "$1" || return 1
  stat=$(<"/proc/$1/stat") || return 1
  stat=${stat##*) }
  [[ "${stat:0:1}" != Z ]]
}

capture_environment() {
  local pid=$1
  [[ -r "/proc/$pid/environ" ]] || {
    printf 'cannot read /proc/%s/environ; refusing to restart the server without its keys\n' "$pid" >&2
    return 1
  }
  mapfile -d '' -t captured_environment <"/proc/$pid/environ"
  ((${#captured_environment[@]} > 0))
}

wait_pid_exit() {
  local pid=$1 limit=$2 attempt
  for ((attempt = 0; attempt < limit * 10; attempt += 1)); do
    server_alive "$pid" || return 0
    sleep 0.1
  done
  return 1
}

wait_walsender_replaced() {
  local slot=$1 expected=$2 deadline current
  deadline=$(($(now_ms) + 10000))
  while (($(now_ms) < deadline)); do
    current=$(sql "SELECT coalesce(active_pid, 0) FROM pg_replication_slots
                   WHERE slot_name = '$slot'" 2>/dev/null || printf 0)
    [[ "$current" != "$expected" ]] && return 0
    sleep 0.2
  done
  return 1
}

wait_walsenders_gone() {
  local limit=$1 deadline count
  deadline=$(($(now_ms) + limit * 1000))
  while (($(now_ms) < deadline)); do
    count=$(sql "SELECT count(*) FROM pg_stat_activity
                 WHERE backend_type = 'walsender'" 2>/dev/null || printf unknown)
    [[ "$count" == 0 ]] && return 0
    sleep 0.2
  done
  return 1
}

kill_server_hard() {
  local pid=$server_pid
  capture_environment "$pid"
  kill -KILL "$pid" 2>/dev/null || true
  wait_pid_exit "$pid" 10 || {
    printf 'server pid %s did not exit after SIGKILL\n' "$pid" >&2
    return 1
  }
  report "phase=stopped at=$(iso_now) action=SIGKILL old_server_pid=$pid"
  # The killed process's walsenders own the fixed-name aggregator slot; a
  # restart that races them fails on the active slot, so wait for PostgreSQL to
  # notice the closed connections first.
  wait_walsenders_gone 30 ||
    fail 'walsenders still own the slot 30s after the server was killed'
}

restart_server() {
  # The captured environment carries the generated keysets; keep it in memory
  # and pass it straight to the new process, as stack.sh did at first start.
  taskset -c "$sut_cpus" env -i "${captured_environment[@]}" \
    "$server_binary" serve >>"$server_log" 2>&1 &
  server_pid=$!
  last_restart_ms=$(now_ms)
  set_stack_pids "server_pid=$server_pid"
  report "phase=restarted at=$(iso_now) server_pid=$server_pid"
}

record_postmaster_pid() {
  local pid
  pid=$(head -n 1 "$data_dir/postmaster.pid" 2>/dev/null || true)
  [[ "$pid" =~ ^[0-9]+$ ]] || fail "could not read the new postmaster pid from $data_dir/postmaster.pid"
  set_stack_pids "postmaster_pid=$pid"
  postmaster_pid=$pid
}

wait_database() {
  local deadline
  deadline=$(($(now_ms) + drill_timeout * 1000))
  while (($(now_ms) < deadline)); do
    database_ok && return 0
    sleep 0.25
  done
  return 1
}

active_invalidator_slot() {
  sql "SELECT slot_name FROM pg_replication_slots
       WHERE slot_name LIKE 'invalidator\\_%' ESCAPE '\\' AND active
       ORDER BY slot_name
       LIMIT 1" 2>/dev/null
}

slot_wal_status() {
  sql "SELECT wal_status FROM pg_replication_slots WHERE slot_name = '$1'" 2>/dev/null
}

slot_summary() {
  sql "SELECT coalesce(
         string_agg(slot_name || '=' || coalesce(wal_status, '?') ||
                    (CASE WHEN active THEN '/active' ELSE '/inactive' END), ' '),
         'none')
       FROM pg_replication_slots WHERE plugin = 'wal2json'" 2>/dev/null
}

wal_directory_bytes() {
  sql 'SELECT coalesce(sum(size), 0) FROM pg_ls_waldir()' 2>/dev/null
}

disk_usage() {
  python3 - "$data_dir" <<'PY'
import shutil
import sys

usage = shutil.disk_usage(sys.argv[1])
print(usage.free, usage.total)
PY
}

emit_wal_mb() {
  local megabytes=$1
  sql "SELECT count(*) FROM (
         SELECT pg_logical_emit_message(
                  false, 'scale-drill',
                  convert_to(repeat('x', 1048576), 'UTF8'))
         FROM generate_series(1, $megabytes)
       ) AS emitted" >/dev/null
  sql 'CHECKPOINT' >/dev/null
}

reset_slot_wal_keep_size() {
  [[ "$slot_keep_altered" == true ]] || return 0
  if sql 'ALTER SYSTEM RESET max_slot_wal_keep_size' >/dev/null 2>&1 &&
    sql 'SELECT pg_reload_conf()' >/dev/null 2>&1; then
    report "phase=restored at=$(iso_now) setting=max_slot_wal_keep_size"
  else
    printf 'could not reset max_slot_wal_keep_size; reset it before reusing the stack:\n' >&2
    printf '  %s %s -X -c "ALTER SYSTEM RESET max_slot_wal_keep_size"\n' \
      "$postgres_bin/psql" "$database_url" >&2
  fi
  slot_keep_altered=false
}

probe_prometheus() {
  prometheus_url=${SCALE_PROMETHEUS_URL:-http://127.0.0.1:9090}
  alerts_checked=false
  alerts_seen=
  if curl --silent --fail --max-time 2 --output /dev/null \
    "$prometheus_url/api/v1/alerts" 2>/dev/null; then
    alerts_checked=true
  fi
}

firing_alerts() {
  curl --silent --fail --max-time 2 "$prometheus_url/api/v1/alerts" |
    python3 -c '
import json
import sys

try:
    payload = json.load(sys.stdin)
except ValueError:
    sys.exit(1)
for alert in payload.get("data", {}).get("alerts", []):
    if alert.get("state") == "firing":
        print(alert.get("labels", {}).get("alertname", ""))
'
}

poll_alerts() {
  [[ "$alerts_checked" == true ]] || return 0
  local name
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    case " $alerts_seen " in
      *" $name "*) ;;
      *) alerts_seen="$alerts_seen $name" ;;
    esac
  done < <(firing_alerts || true)
}

alert_result() {
  if [[ "$alerts_checked" != true ]]; then
    report "phase=alerts at=$(iso_now) observed=unavailable prometheus=$prometheus_url expected=${alert_names[*]}"
    return 0
  fi
  local name fired=
  for name in "${alert_names[@]}"; do
    case " $alerts_seen " in
      *" $name "*) fired="$fired $name" ;;
    esac
  done
  report "phase=alerts at=$(iso_now) observed=prometheus fired=${fired:-none} expected=${alert_names[*]}"
}

baseline() {
  server_alive "$server_pid" || fail "recorded server pid $server_pid is not running"
  health_ok || fail "server is not healthy at $origin_url/health/system"
  database_ok || fail "cannot query PostgreSQL at the recorded database URL"
  canary_ok || fail 'no active wal2json invalidator slot is consuming WAL'
  local backends senders
  backends=$(sql "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend'" 2>/dev/null || printf unknown)
  senders=$(sql "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'walsender'" 2>/dev/null || printf unknown)
  report "phase=baseline at=$(iso_now) server_pid=$server_pid client_backends=$backends walsenders=$senders slots=$(slot_summary)"
}

wait_recovery() {
  local fault_ms=$1 label=$2
  # The recovery window starts when the fault has been injected and any hold
  # (disk-pressure) has elapsed, not at the fault itself.
  local deadline_ms=$(($(now_ms) + drill_timeout * 1000))
  local health_ms= canary_ms= next_alert_ms=0 now
  while :; do
    now=$(now_ms)
    if ((now >= deadline_ms)); then
      log_tail
      fail "no recovery from $label within ${drill_timeout}s"
    fi
    if ! server_alive "$server_pid"; then
      log_tail
      fail "server pid $server_pid exited while recovering from $label"
    fi
    if [[ -z "$health_ms" ]] && health_ok; then
      health_ms=$now
      report "phase=healthy at=$(iso_now) health_seconds=$(elapsed_seconds "$fault_ms")"
    fi
    if [[ -n "$health_ms" && -z "$canary_ms" ]] && canary_ok; then
      canary_ms=$(now_ms)
      report "phase=canary at=$(iso_now) canary_seconds=$(elapsed_seconds "$fault_ms")"
      local since_restart=
      if [[ -n "$last_restart_ms" ]]; then
        since_restart=" since_restart_seconds=$(elapsed_seconds "$last_restart_ms")"
      fi
      report "phase=recovered reason=$label at=$(iso_now) recovery_seconds=$(elapsed_seconds "$fault_ms")$since_restart"
      alert_result
      return 0
    fi
    if ((now >= next_alert_ms)); then
      poll_alerts
      next_alert_ms=$((now + alert_interval * 1000))
    fi
    sleep 0.5
  done
}

drill_kill9() {
  alert_names=(ServiceUnavailable)
  baseline
  local fault_ms
  fault_ms=$(now_ms)
  report "phase=fault at=$(iso_now) action=SIGKILL"
  kill_server_hard
  report "phase=supervisor at=$(iso_now) note=scale-local-has-no-supervisor-restarting-recorded-binary"
  restart_server
  wait_recovery "$fault_ms" server-crash
}

active_invalidator_walsender() {
  sql "SELECT slot_name, coalesce(active_pid, 0)
       FROM pg_replication_slots
       WHERE slot_name LIKE 'invalidator\\_%' ESCAPE '\\'
         AND active
       ORDER BY slot_name
       LIMIT 1" 2>/dev/null
}

terminate_invalidator_walsender() {
  local label=$1 slot_name= walsender_pid=
  IFS='|' read -r slot_name walsender_pid < <(active_invalidator_walsender) || true
  [[ "$walsender_pid" =~ ^[0-9]+$ ]] && ((walsender_pid > 0)) ||
    fail "no active invalidator walsender to terminate (slots: $(slot_summary))"
  sql "SELECT pg_terminate_backend($walsender_pid)" >/dev/null
  report "phase=fault at=$(iso_now) action=terminate_walsender iteration=$label walsender_pid=$walsender_pid"
  # Do not count the fault as recovered while the terminated backend is
  # still visible: the replacement pid (or no pid) proves the fault landed.
  wait_walsender_replaced "$slot_name" "$walsender_pid" ||
    fail "walsender $walsender_pid was not replaced within 10s"
}

# Up to three producer restarts in ten minutes clear health on reconnect; each
# must recover within the drill timeout. One more restart inside the window is
# a flapping stream: readiness must be withheld while the stream itself keeps
# working, and must clear by itself once the burst ages out of the window.
drill_walsender() {
  alert_names=(ServiceUnavailable ReplicationSlotInactive InvalidationStreamFlapping)
  baseline
  local iteration fault_ms
  for ((iteration = 1; iteration <= walsender_kills; iteration += 1)); do
    fault_ms=$(now_ms)
    terminate_invalidator_walsender "$iteration"
    wait_recovery "$fault_ms" "walsender-$iteration"
  done
  ((walsender_flap_timeout > 0)) || return 0

  fault_ms=$(now_ms)
  terminate_invalidator_walsender flap
  local deadline_ms=$(($(now_ms) + 30000))
  until canary_ok; do
    (($(now_ms) < deadline_ms)) || fail "invalidator stream did not reconnect within 30s after the flap restart"
    sleep 0.5
  done
  sleep 6 # one health evaluation tick after the reconnect
  if health_ok; then
    fail "health was not withheld after $((walsender_kills + 1)) restarts inside the flap window"
  fi
  report "phase=flap-withheld at=$(iso_now) stream=connected health=withheld"
  deadline_ms=$(($(now_ms) + walsender_flap_timeout * 1000))
  until health_ok; do
    if (($(now_ms) >= deadline_ms)); then
      log_tail
      fail "health did not clear by itself within ${walsender_flap_timeout}s after the flap"
    fi
    server_alive "$server_pid" || fail "server pid $server_pid exited while flap health was withheld"
    sleep 2
  done
  report "phase=flap-cleared at=$(iso_now) clear_seconds=$(elapsed_seconds "$fault_ms")"
  alert_result
}

drill_pg_restart() {
  alert_names=(DatabaseUnavailable ServiceUnavailable)
  baseline
  local fault_ms
  fault_ms=$(now_ms)
  report "phase=fault at=$(iso_now) action=pg_ctl-fast-restart"
  if ! taskset -c "$sut_cpus" "$postgres_bin/pg_ctl" -D "$data_dir" \
    -l "$postgres_log" -m fast -w -t 60 restart >/dev/null 2>&1; then
    fail "pg_ctl restart failed; see $postgres_log"
  fi
  record_postmaster_pid
  report "phase=postgres-restarted at=$(iso_now) postmaster_pid=$postmaster_pid"
  wait_database || fail "PostgreSQL did not accept queries within ${drill_timeout}s"
  wait_recovery "$fault_ms" postgres-restart
}

drill_invalidate_slot() {
  alert_names=(RequiredSlotInvalid ReplicationSlotUnhealthy ServiceUnavailable)
  baseline
  local slot fault_ms status
  slot=$(active_invalidator_slot)
  [[ -n "$slot" ]] || fail "no active invalidator slot to invalidate (slots: $(slot_summary))"
  report "phase=prepared at=$(iso_now) slot=$slot max_slot_wal_keep_size=$slot_wal_keep_size"
  sql "ALTER SYSTEM SET max_slot_wal_keep_size = '$slot_wal_keep_size'" >/dev/null
  slot_keep_altered=true
  sql 'SELECT pg_reload_conf()' >/dev/null

  fault_ms=$(now_ms)
  report "phase=fault at=$(iso_now) action=SIGKILL reason=invalidate-slot"
  kill_server_hard

  local round total_mb=0
  for ((round = 1; round <= wal_rounds; round += 1)); do
    emit_wal_mb "$wal_round_mb"
    total_mb=$((total_mb + wal_round_mb))
    status=$(slot_wal_status "$slot")
    report "phase=wal at=$(iso_now) generated_mb=$total_mb slot_wal_status=${status:-absent}"
    [[ "$status" == lost ]] && break
  done
  [[ "$status" == lost ]] || fail "slot $slot did not become invalid after ${total_mb}MB of WAL (wal_status=${status:-absent})"
  report "phase=invalidated at=$(iso_now) slot=$slot generated_mb=$total_mb"

  # Restore the cap before the restart so the fresh slot is not invalidated by
  # the next few megabytes of load traffic.
  reset_slot_wal_keep_size
  restart_server
  wait_recovery "$fault_ms" invalidated-slot
  report "phase=slot-state at=$(iso_now) slots=$(slot_summary)"
}

drill_disk_pressure() {
  alert_names=(WalRetentionHigh DiskHeadroom RequiredSlotInvalid)
  baseline
  local slot fault_ms status
  slot=$(active_invalidator_slot)
  [[ -n "$slot" ]] || fail "no active invalidator slot to cap (slots: $(slot_summary))"
  sql "ALTER SYSTEM SET max_slot_wal_keep_size = '$disk_pressure_keep_size'" >/dev/null
  slot_keep_altered=true
  sql 'SELECT pg_reload_conf()' >/dev/null
  report "phase=prepared at=$(iso_now) slot=$slot max_slot_wal_keep_size=$disk_pressure_keep_size hold_seconds=$disk_pressure_seconds"

  fault_ms=$(now_ms)
  report "phase=fault at=$(iso_now) action=SIGKILL reason=disk-pressure"
  kill_server_hard

  local hold_end=$((fault_ms + disk_pressure_seconds * 1000))
  local peak_wal=0 min_free= disk_total= lost_at= remaining_ms sleep_seconds
  local free_bytes wal_bytes
  while :; do
    remaining_ms=$((hold_end - $(now_ms)))
    ((remaining_ms > 0)) || break
    emit_wal_mb "$disk_pressure_wal_mb"
    poll_alerts
    status=$(slot_wal_status "$slot")
    if ! read -r free_bytes disk_total < <(disk_usage); then
      free_bytes=
      disk_total=
    fi
    if [[ "$free_bytes" =~ ^[0-9]+$ ]] &&
      [[ ! "$min_free" =~ ^[0-9]+$ || "$free_bytes" -lt "$min_free" ]]; then
      min_free=$free_bytes
    fi
    wal_bytes=$(wal_directory_bytes)
    wal_bytes=${wal_bytes:-0}
    if ((wal_bytes > peak_wal)); then
      peak_wal=$wal_bytes
    fi
    if [[ "$status" == lost && -z "$lost_at" ]]; then
      lost_at=$(iso_now)
    fi
    report "phase=hold at=$(iso_now) elapsed_seconds=$(elapsed_seconds "$fault_ms") wal_bytes=$wal_bytes free_bytes=${free_bytes:-unknown} slot_wal_status=${status:-absent}"
    sleep_seconds=$((remaining_ms / 1000))
    ((sleep_seconds > disk_pressure_tick)) && sleep_seconds=$disk_pressure_tick
    ((sleep_seconds > 0)) || sleep_seconds=1
    sleep "$sleep_seconds"
  done

  local headroom=unknown
  if [[ "$min_free" =~ ^[0-9]+$ && "$disk_total" =~ ^[0-9]+$ ]] &&
    ((disk_total > 0)); then
    if ((min_free < 10737418240)) ||
      ((min_free * 100 / disk_total < 15)); then
      headroom=below-threshold
    else
      headroom=above-threshold
    fi
  fi
  report "phase=held at=$(iso_now) peak_wal_bytes=$peak_wal min_free_bytes=${min_free:-unknown} disk_headroom=$headroom slot_invalidated_at=${lost_at:-never} slot_wal_status=${status:-absent}"

  # The cap did its job while the server was down; remove it before the
  # restart so the replacement slot has the normal retention budget.
  reset_slot_wal_keep_size
  restart_server
  wait_recovery "$fault_ms" disk-pressure
  report "phase=slot-state at=$(iso_now) slots=$(slot_summary)"
}

cleanup_exit() {
  local status=$?
  trap - EXIT
  reset_slot_wal_keep_size
  exit "$status"
}

run_drill() {
  local name=$1
  shift
  drill_name=$name
  load_stack
  probe_prometheus
  trap cleanup_exit EXIT
  drill_started_ms=$(now_ms)
  last_restart_ms=
  report "phase=start at=$(iso_now) stack=$stack_json"
  "drill_$name" "$@"
  reset_slot_wal_keep_size
  report "phase=end at=$(iso_now) duration_seconds=$(elapsed_seconds "$drill_started_ms")"
}

main() {
  local subcommand=${1:-}
  (($#)) && shift
  case "$subcommand" in
    kill9 | walsender | pg-restart | invalidate-slot | disk-pressure)
      run_drill "${subcommand//-/_}" "$@"
      ;;
    -h | --help) drills_usage ;;
    *)
      drills_usage >&2
      exit 2
      ;;
  esac
}

main "$@"
