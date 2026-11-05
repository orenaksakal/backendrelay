#!/usr/bin/env bash
set -Eeuo pipefail

# Brings up and tears down a reproducible local "system under test" for load
# tests of 10,000 concurrent users across 10 apps: a disposable PostgreSQL
# 17.11 cluster and a release-built instant-server, both pinned to a dedicated
# CPU set so a later load-generator step can own the remaining CPUs.
#
# Usage:
#   stack.sh up        provision and start Postgres + the Rust server
#   stack.sh down      stop both and delete the data directory (--keep to retain)
#   stack.sh status    print stack.json and report which pids are alive
#
# State lives in SCALE_STATE_DIR (default under XDG_STATE_HOME). Logs are kept
# there. Generated secrets are never written to disk, stack.json, or stdout.

readonly POSTGRES_VERSION=17.11
readonly WAL2JSON_TAG=wal2json_2_6
readonly PG_HINT_PLAN_TAG=REL17_1_7_2
# The load test opens ~10,500 websockets plus pool connections; the production
# compose server service raises its nofile soft limit to the same value.
readonly min_open_files=65536

repo_root=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
server_root="$repo_root/server-rs"
state_dir="${SCALE_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/instant-scale-local/current}"

stack_json="$state_dir/stack.json"
data_dir="$state_dir/data"
postgres_log="$state_dir/postgres.log"
server_log="$state_dir/server.log"

# Long-running steps are bounded; each override below is exposed to the caller.
readonly setup_timeout=${SCALE_SETUP_TIMEOUT:-10m}
readonly build_timeout=${SCALE_BUILD_TIMEOUT:-40m}
readonly migrate_timeout=${SCALE_MIGRATE_TIMEOUT:-10m}
readonly readiness_seconds=${SCALE_READINESS_SECONDS:-180}
readonly stop_grace_seconds=${SCALE_STOP_GRACE_SECONDS:-30}

usage() {
  cat <<'EOF'
usage: stack.sh up | down [--keep] | status

  up       provision disposable PostgreSQL 17.11 and a release instant-server,
           pin both to SCALE_SUT_CPUS, and record state in stack.json.
  down     stop the server and Postgres (graceful, then forced), then delete
           the data directory. Pass --keep to retain the data directory.
  status   print stack.json and whether the recorded pids are alive.

Environment overrides (defaults mirror the production compose tuning):
  SCALE_STATE_DIR, SCALE_SUT_CPUS, SCALE_SUT_PHYSICAL_CORES, SCALE_PG_EXTRA_CONF,
  SCALE_SERVER_BINARY (prebuilt server; skips the build),
  SCALE_GENERATOR_CPUS (reported), CONNECTION_POOL_SIZE, TOKIO_WORKER_THREADS,
  INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS, INSTANT_SKETCH_BATCH_MAX_WAL_BYTES,
  INSTANT_SKETCH_BATCH_MAX_WAIT_MS, WAL_HISTORY_STORAGE, and the S3_* /
  INSTANT_S3_* variables (dummy local defaults; the load test never uses file
  storage and startup does not contact S3).
EOF
}

run_bounded() {
  local label=$1 duration=$2
  shift 2
  echo "==> $label (limit: $duration)"
  timeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
}

# Derives the SUT and generator CPU sets. If SCALE_SUT_CPUS is set it is used
# verbatim; otherwise the logical CPUs of the first SCALE_SUT_PHYSICAL_CORES
# physical cores are assigned to the SUT. In both cases the generator set is the
# complement over every online logical CPU. Output: two compact list lines.
derive_cpus() {
  python3 - <<'PY'
import os
import subprocess

def compact(cpus):
    cpus = sorted(set(cpus))
    if not cpus:
        return ""
    ranges = []
    start = prev = cpus[0]
    for cpu in cpus[1:]:
        if cpu == prev + 1:
            prev = cpu
        else:
            ranges.append((start, prev))
            start = prev = cpu
    ranges.append((start, prev))
    parts = [str(s) if s == e else f"{s}-{e}" for s, e in ranges]
    return ",".join(parts)

def expand(spec):
    cpus = set()
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            lo, hi = part.split("-", 1)
            cpus.update(range(int(lo), int(hi) + 1))
        else:
            cpus.add(int(part))
    return cpus

out = subprocess.check_output(["lscpu", "-p=CPU,CORE"], text=True)
pairs = []
for line in out.splitlines():
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    cpu, core = line.split(",")[:2]
    pairs.append((int(cpu), int(core)))

all_cpus = [cpu for cpu, _ in pairs]
cores = {}
for cpu, core in pairs:
    cores.setdefault(core, []).append(cpu)

override = os.environ.get("SCALE_SUT_CPUS", "").strip()
if override:
    sut = expand(override)
else:
    count = int(os.environ.get("SCALE_SUT_PHYSICAL_CORES", "8"))
    sut_cores = sorted(cores)[:count]
    sut = {cpu for core in sut_cores for cpu in cores[core]}

sut &= set(all_cpus)
generator = [cpu for cpu in all_cpus if cpu not in sut]
print(compact(sorted(sut)))
print(compact(generator))
PY
}

# pg_hint_plan generates its hint scanner with flex, which the mise PostgreSQL
# build does not ship. Prefer a host flex/m4; otherwise unpack the distro
# packages into the PostgreSQL cache (no root required) so `up` stays usable on
# hosts without them. Prints a bin directory holding both, or nothing when the
# host toolchain already provides them.
ensure_flex() {
  local postgres_root=$1 toolchain download package
  if command -v flex >/dev/null 2>&1 && command -v m4 >/dev/null 2>&1; then
    return 0
  fi
  toolchain="$postgres_root/instant-acceptance-cache/flex"
  if [[ ! -x "$toolchain/usr/bin/flex" || ! -x "$toolchain/usr/bin/m4" ]]; then
    download="$toolchain/download"
    rm -rf -- "$download" "${toolchain:?}/usr"
    mkdir -p "$download"
    if ! (cd -- "$download" && run_bounded "download flex build dependency" \
        "$setup_timeout" apt-get download flex m4) >&2; then
      echo "flex and m4 are required to build pg_hint_plan; install them and retry" >&2
      return 1
    fi
    for package in "$download"/flex_*.deb "$download"/m4_*.deb; do
      [[ -f "$package" ]] || continue
      dpkg-deb --extract "$package" "$toolchain"
    done
    if [[ ! -x "$toolchain/usr/bin/flex" || ! -x "$toolchain/usr/bin/m4" ]]; then
      echo "could not unpack flex/m4 for the pg_hint_plan build" >&2
      return 1
    fi
  fi
  echo "$toolchain/usr/bin"
}

pid_alive() {
  kill -0 "$1" 2>/dev/null
}

stop_server() {
  local pid=${1:-}
  [[ -n "$pid" ]] || return 0
  pid_alive "$pid" || return 0
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 $((stop_grace_seconds * 10))); do
    pid_alive "$pid" || return 0
    sleep 0.1
  done
  if pid_alive "$pid"; then
    echo "server pid $pid did not stop within ${stop_grace_seconds}s; forcing" >&2
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

stop_postgres() {
  local bin=$1 pgdata=$2
  [[ -n "$bin" && -f "$pgdata/postmaster.pid" ]] || return 0
  "$bin/pg_ctl" -D "$pgdata" -m fast -w -t "$stop_grace_seconds" stop >/dev/null 2>&1 || true
  if [[ -f "$pgdata/postmaster.pid" ]]; then
    "$bin/pg_ctl" -D "$pgdata" -m immediate -w stop >/dev/null 2>&1 || true
  fi
}

up() {
  local command
  for command in mise git make python3 cargo curl taskset lscpu timeout; do
    command -v "$command" >/dev/null 2>&1 || {
      echo "required command is not installed: $command" >&2
      exit 1
    }
  done

  [[ ! -e "$stack_json" ]] || {
    echo "stack already present at $stack_json; run 'stack.sh down' first" >&2
    exit 1
  }

  mkdir -p "$state_dir"
  : >"$server_log"
  : >"$postgres_log"

  local postgres_bin server_pid postmaster_pid
  server_pid=
  postmaster_pid=

  cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ "$status" != 0 ]]; then
      [[ -n "$server_pid" ]] && stop_server "$server_pid"
      [[ -n "${postgres_bin:-}" ]] && stop_postgres "$postgres_bin" "$data_dir"
      if [[ -s "$server_log" ]]; then
        echo "Last server log lines:" >&2
        tail -n 200 "$server_log" >&2
      fi
      if [[ -s "$postgres_log" ]]; then
        echo "Last PostgreSQL log lines:" >&2
        tail -n 100 "$postgres_log" >&2
      fi
    fi
    exit "$status"
  }
  trap cleanup EXIT INT TERM

  # --- File descriptors ----------------------------------------------------
  # Postgres and the server inherit this shell's soft limit, so raise it once
  # here rather than after either process has started.
  local nofile_soft nofile_hard
  nofile_soft=$(ulimit -Sn)
  nofile_hard=$(ulimit -Hn)
  if [[ "$nofile_hard" != unlimited ]] && ((nofile_hard < min_open_files)); then
    echo "open-file hard limit is $nofile_hard, below the required $min_open_files" >&2
    echo "raise it (for example 'ulimit -Hn $min_open_files') and retry" >&2
    exit 1
  fi
  if [[ "$nofile_soft" != unlimited ]] && ((nofile_soft < min_open_files)); then
    # Only the soft limit is raised; the hard limit stays as the operator set it.
    ulimit -S -n "$min_open_files" || {
      echo "could not raise the open-file soft limit to $min_open_files" >&2
      exit 1
    }
    echo "==> open-file soft limit raised to $(ulimit -Sn) (hard: $nofile_hard)"
  fi

  # --- CPU sets ------------------------------------------------------------
  local sut_cpus generator_cpus
  {
    read -r sut_cpus
    read -r generator_cpus
  } < <(derive_cpus)
  [[ -n "$sut_cpus" ]] || { echo "could not derive a SUT CPU set" >&2; exit 1; }
  echo "==> SUT CPUs: $sut_cpus ; generator CPUs: ${generator_cpus:-none}"

  # --- PostgreSQL 17.11 + wal2json -----------------------------------------
  run_bounded "install PostgreSQL $POSTGRES_VERSION" "$setup_timeout" \
    mise install "postgres@$POSTGRES_VERSION"
  postgres_root=$(mise where "postgres@$POSTGRES_VERSION")
  postgres_bin="$postgres_root/bin"
  for command in initdb pg_ctl createdb psql; do
    [[ -x "$postgres_bin/$command" ]] || {
      echo "PostgreSQL $POSTGRES_VERSION command is unavailable: $postgres_bin/$command" >&2
      exit 1
    }
  done

  local wal2json_library="$postgres_root/lib/wal2json.so"
  if [[ ! -f "$wal2json_library" && ! -f "$postgres_root/lib/wal2json.dylib" ]]; then
    local wal2json_cache="$postgres_root/instant-acceptance-cache/wal2json-$WAL2JSON_TAG"
    if [[ ! -d "$wal2json_cache/.git" ]]; then
      mkdir -p "$(dirname -- "$wal2json_cache")"
      run_bounded "download wal2json $WAL2JSON_TAG" "$setup_timeout" \
        git clone --depth 1 --branch "$WAL2JSON_TAG" \
          https://github.com/eulerto/wal2json.git "$wal2json_cache"
    fi
    run_bounded "build wal2json $WAL2JSON_TAG" "$setup_timeout" \
      make -C "$wal2json_cache" PG_CONFIG="$postgres_bin/pg_config"
    run_bounded "install wal2json $WAL2JSON_TAG" "$setup_timeout" \
      make -C "$wal2json_cache" PG_CONFIG="$postgres_bin/pg_config" install
  fi

  # Production preloads pg_hint_plan, and the server's query hints are silently
  # ignored without it, so build the matching PG17 release into the same prefix.
  local pg_hint_plan_library="$postgres_root/lib/pg_hint_plan.so"
  if [[ ! -f "$pg_hint_plan_library" && ! -f "$postgres_root/lib/pg_hint_plan.dylib" ]]; then
    local pg_hint_plan_cache="$postgres_root/instant-acceptance-cache/pg_hint_plan-$PG_HINT_PLAN_TAG"
    if [[ ! -d "$pg_hint_plan_cache/.git" ]]; then
      mkdir -p "$(dirname -- "$pg_hint_plan_cache")"
      run_bounded "download pg_hint_plan $PG_HINT_PLAN_TAG" "$setup_timeout" \
        git clone --depth 1 --branch "$PG_HINT_PLAN_TAG" \
          https://github.com/ossc-db/pg_hint_plan.git "$pg_hint_plan_cache"
    fi
    local flex_dir flex_make_args=()
    flex_dir=$(ensure_flex "$postgres_root") || exit 1
    if [[ -n "$flex_dir" ]]; then
      flex_make_args=("FLEX=$flex_dir/flex" "M4=$flex_dir/m4")
    fi
    # The mise PostgreSQL is a conda-forge build whose pg_config names a
    # compiler that is not installed here; use the host C compiler instead.
    run_bounded "build pg_hint_plan $PG_HINT_PLAN_TAG" "$setup_timeout" \
      make -C "$pg_hint_plan_cache" PG_CONFIG="$postgres_bin/pg_config" CC=cc \
        ${flex_make_args[@]+"${flex_make_args[@]}"}
    run_bounded "install pg_hint_plan $PG_HINT_PLAN_TAG" "$setup_timeout" \
      make -C "$pg_hint_plan_cache" PG_CONFIG="$postgres_bin/pg_config" install
  fi

  local postgres_port
  postgres_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
  local server_port
  server_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
  while [[ "$server_port" == "$postgres_port" ]]; do
    server_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
  done

  run_bounded "initialize disposable PostgreSQL" "$setup_timeout" \
    "$postgres_bin/initdb" --pgdata="$data_dir" --username=instant \
      --auth=trust --encoding=UTF8 --no-locale >/dev/null

  # Tune like the production compose Postgres service, including the preloaded
  # extensions its query hints and statistics rely on.
  printf '%s\n' \
    "listen_addresses = '127.0.0.1'" \
    "port = $postgres_port" \
    "unix_socket_directories = '$state_dir'" \
    "wal_level = logical" \
    "max_connections = 200" \
    "max_replication_slots = 20" \
    "max_wal_senders = 20" \
    "shared_preload_libraries = 'pg_hint_plan,pg_stat_statements'" \
    "output_plugin_libraries = 'wal2json'" \
    "shared_buffers = 2GB" \
    "effective_cache_size = 6GB" \
    "maintenance_work_mem = 128MB" \
    "work_mem = 4MB" \
    "max_worker_processes = 8" \
    "max_parallel_workers = 4" \
    "max_parallel_workers_per_gather = 1" \
    "random_page_cost = 1.1" \
    "track_io_timing = on" \
    "track_wal_io_timing = on" \
    "fsync = on" \
    "synchronous_commit = on" \
    "full_page_writes = on" \
    "timezone = 'UTC'" \
    >>"$data_dir/postgresql.conf"
  # Diagnostic runs may append settings (later lines win), e.g. auto_explain.
  if [[ -n "${SCALE_PG_EXTRA_CONF:-}" ]]; then
    printf '%s\n' "$SCALE_PG_EXTRA_CONF" >>"$data_dir/postgresql.conf"
  fi

  run_bounded "start disposable PostgreSQL" "$setup_timeout" \
    taskset -c "$sut_cpus" "$postgres_bin/pg_ctl" -D "$data_dir" \
      -l "$postgres_log" -w start >/dev/null
  postmaster_pid=$(head -n 1 "$data_dir/postmaster.pid")

  run_bounded "create disposable database" "$setup_timeout" \
    "$postgres_bin/createdb" -h 127.0.0.1 -p "$postgres_port" -U instant instant
  "$postgres_bin/psql" -h 127.0.0.1 -p "$postgres_port" -U instant -d instant \
    -X -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null

  database_url="postgresql://instant@127.0.0.1:$postgres_port/instant?sslmode=disable"
  server_origin="http://127.0.0.1:$server_port"

  # --- Server binary -------------------------------------------------------
  local binary="$server_root/target/release/instant-server"
  if [[ -n "${SCALE_SERVER_BINARY:-}" ]]; then
    # Prebuilt binary (e.g. a frame-pointer profiling build); skip the build.
    binary=$SCALE_SERVER_BINARY
    echo "==> using prebuilt server binary $binary"
  else
    # The single-quoted body is evaluated by `bash -c`; `$1` must not expand here.
    # shellcheck disable=SC2016
    run_bounded "build release instant-server" "$build_timeout" \
      bash -c 'cd -- "$1" && exec cargo build --locked --release -p instant-server --no-default-features' \
        _ "$server_root"
  fi
  [[ -x "$binary" ]] || { echo "release binary missing: $binary" >&2; exit 1; }

  local git_commit
  git_commit=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || printf unavailable)

  # --- Secrets, migrate, serve --------------------------------------------
  # The generated key material is captured, validated, and exported in-memory;
  # it is never written to disk or to stack.json.
  local generated line
  local aead="" webhook="" rate_limit=""
  generated=$("$binary" generate-secrets) || {
    echo "generate-secrets failed" >&2
    exit 1
  }
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      INSTANT_AEAD_KEYSET_JSON=\'*\')
        aead=${line#INSTANT_AEAD_KEYSET_JSON=\'}
        aead=${aead%\'}
        ;;
      INSTANT_WEBHOOK_KEYSET_JSON=\'*\')
        webhook=${line#INSTANT_WEBHOOK_KEYSET_JSON=\'}
        webhook=${webhook%\'}
        ;;
      INSTANT_RATE_LIMIT_HMAC_KEY=*)
        rate_limit=${line#INSTANT_RATE_LIMIT_HMAC_KEY=}
        ;;
      *)
        echo "unexpected generate-secrets output" >&2
        exit 1
        ;;
    esac
  done <<<"$generated"
  [[ -n "$aead" && -n "$webhook" && -n "$rate_limit" ]] || {
    echo "incomplete generate-secrets output" >&2
    exit 1
  }
  export INSTANT_AEAD_KEYSET_JSON="$aead"
  export INSTANT_WEBHOOK_KEYSET_JSON="$webhook"
  export INSTANT_RATE_LIMIT_HMAC_KEY="$rate_limit"

  # Server environment mirrors the production compose runtime, each overridable from
  # the caller's environment. S3 values are dummy local values: startup only
  # constructs an S3 client handle and never contacts the endpoint.
  local server_env=(
    "DATABASE_URL=$database_url"
    "PORT=$server_port"
    "INSTANT_BACKEND_URL=$server_origin"
    "INSTANT_DASHBOARD_URL=http://localhost:3000"
    # The load generator seeds fixture apps through the ephemeral-app route,
    # which production keeps closed by default.
    "INSTANT_EPHEMERAL_APPS=${INSTANT_EPHEMERAL_APPS:-true}"
    "WAL_HISTORY_STORAGE=${WAL_HISTORY_STORAGE:-pg}"
    "CONNECTION_POOL_SIZE=${CONNECTION_POOL_SIZE:-48}"
    "INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS=${INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS:-1024}"
    "INSTANT_SKETCH_BATCH_MAX_WAL_BYTES=${INSTANT_SKETCH_BATCH_MAX_WAL_BYTES:-16777216}"
    "INSTANT_SKETCH_BATCH_MAX_WAIT_MS=${INSTANT_SKETCH_BATCH_MAX_WAIT_MS:-1000}"
    "S3_ENDPOINT=${S3_ENDPOINT:-http://127.0.0.1:9000}"
    "S3_PUBLIC_ENDPOINT=${S3_PUBLIC_ENDPOINT:-http://127.0.0.1:9000}"
    "INSTANT_S3_REGION=${INSTANT_S3_REGION:-us-east-1}"
    "S3_BUCKET=${S3_BUCKET:-instant-scale-test}"
    "S3_APP_BACKUPS_BUCKET=${S3_APP_BACKUPS_BUCKET:-instant-scale-test-backups}"
    "INSTANT_S3_ACCESS_KEY_ID=${INSTANT_S3_ACCESS_KEY_ID:-scale-test}"
    "INSTANT_S3_SECRET_ACCESS_KEY=${INSTANT_S3_SECRET_ACCESS_KEY:-scale-test-secret}"
  )
  # TOKIO_WORKER_THREADS is accepted by the operator contract; the native
  # binary sizes its Tokio runtime from the CPU set when left unset.
  if [[ -n "${TOKIO_WORKER_THREADS:-}" ]]; then
    server_env+=("TOKIO_WORKER_THREADS=$TOKIO_WORKER_THREADS")
  fi

  run_bounded "migrate disposable PostgreSQL" "$migrate_timeout" \
    env "${server_env[@]}" "$binary" migrate up

  echo "==> start instant-server on $server_origin (pinned to $sut_cpus)"
  taskset -c "$sut_cpus" env "${server_env[@]}" "$binary" serve >"$server_log" 2>&1 &
  server_pid=$!

  echo "==> wait for server readiness (limit: ${readiness_seconds}s)"
  local ready=false attempt
  for ((attempt = 0; attempt < readiness_seconds; attempt += 1)); do
    if ! pid_alive "$server_pid"; then
      echo "instant-server exited before readiness" >&2
      exit 1
    fi
    if curl --silent --show-error --fail --max-time 2 \
        "http://127.0.0.1:$server_port/health/system" >/dev/null 2>&1; then
      ready=true
      break
    fi
    sleep 1
  done
  [[ "$ready" == true ]] || {
    echo "instant-server did not become healthy within ${readiness_seconds}s" >&2
    exit 1
  }

  SCALE_JSON_ORIGIN="$server_origin" \
  SCALE_JSON_SERVER_PID="$server_pid" \
  SCALE_JSON_POSTMASTER_PID="$postmaster_pid" \
  SCALE_JSON_POSTGRES_PORT="$postgres_port" \
  SCALE_JSON_POSTGRES_BIN="$postgres_bin" \
  SCALE_JSON_DATABASE_URL="$database_url" \
  SCALE_JSON_DATA_DIR="$data_dir" \
  SCALE_JSON_SERVER_LOG="$server_log" \
  SCALE_JSON_POSTGRES_LOG="$postgres_log" \
  SCALE_JSON_SUT_CPUS="$sut_cpus" \
  SCALE_JSON_GENERATOR_CPUS="$generator_cpus" \
  SCALE_JSON_SERVER_BIN="$binary" \
  SCALE_JSON_GIT_COMMIT="$git_commit" \
  python3 - "$stack_json" <<'PY'
import json
import os
import sys

payload = {
    "origin_url": os.environ["SCALE_JSON_ORIGIN"],
    "server_pid": int(os.environ["SCALE_JSON_SERVER_PID"]),
    "postmaster_pid": int(os.environ["SCALE_JSON_POSTMASTER_PID"]),
    "postgres_port": int(os.environ["SCALE_JSON_POSTGRES_PORT"]),
    "postgres_bin": os.environ["SCALE_JSON_POSTGRES_BIN"],
    "database_url": os.environ["SCALE_JSON_DATABASE_URL"],
    "data_dir": os.environ["SCALE_JSON_DATA_DIR"],
    "server_log": os.environ["SCALE_JSON_SERVER_LOG"],
    "postgres_log": os.environ["SCALE_JSON_POSTGRES_LOG"],
    "sut_cpus": os.environ["SCALE_JSON_SUT_CPUS"],
    "generator_cpus": os.environ["SCALE_JSON_GENERATOR_CPUS"],
    "server_binary": os.environ["SCALE_JSON_SERVER_BIN"],
    "git_commit": os.environ["SCALE_JSON_GIT_COMMIT"],
}
with open(sys.argv[1], "w") as handle:
    json.dump(payload, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

  trap - EXIT INT TERM
  echo "stack up:"
  cat "$stack_json"
}

down() {
  local keep=false
  while (($#)); do
    case "$1" in
      --keep) keep=true ;;
      *) usage >&2; exit 2 ;;
    esac
    shift
  done

  [[ -e "$stack_json" ]] || {
    echo "no stack recorded at $stack_json; nothing to stop" >&2
    exit 1
  }

  local server_pid postmaster_pid postgres_bin
  server_pid=$(python3 - "$stack_json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1])).get("server_pid", ""))
PY
)
  postmaster_pid=$(python3 - "$stack_json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1])).get("postmaster_pid", ""))
PY
)
  postgres_bin=$(mise where "postgres@$POSTGRES_VERSION" 2>/dev/null || true)
  postgres_bin="${postgres_bin:+$postgres_bin/bin}"

  [[ -n "$server_pid" ]] && stop_server "$server_pid"
  if [[ -n "$postmaster_pid" && -n "${postgres_bin:-}" && -f "$data_dir/postmaster.pid" ]]; then
    stop_postgres "$postgres_bin" "$data_dir"
  fi

  if [[ "$keep" == false ]]; then
    rm -rf -- "$data_dir"
    echo "removed data directory: $data_dir"
  else
    echo "kept data directory: $data_dir"
  fi

  rm -f -- "$stack_json"
  echo "stack down"
}

status() {
  [[ -e "$stack_json" ]] || {
    echo "no stack recorded at $stack_json"
    exit 0
  }
  cat "$stack_json"
  echo
  local server_pid postmaster_pid
  server_pid=$(python3 - "$stack_json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1])).get("server_pid", ""))
PY
)
  postmaster_pid=$(python3 - "$stack_json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1])).get("postmaster_pid", ""))
PY
)
  printf 'server pid %s: ' "$server_pid"
  if [[ -n "$server_pid" ]] && pid_alive "$server_pid"; then echo alive; else echo dead; fi
  printf 'postmaster pid %s: ' "$postmaster_pid"
  if [[ -n "$postmaster_pid" ]] && pid_alive "$postmaster_pid"; then echo alive; else echo dead; fi
}

main() {
  local subcommand=${1:-}
  (($#)) && shift
  case "$subcommand" in
    up) up ;;
    down) down "$@" ;;
    status) status ;;
    -h|--help) usage ;;
    *) usage >&2; exit 2 ;;
  esac
}

# Sourced by drills.sh for pid_alive and the shared paths; its CLI only runs
# when it is the executed script.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
