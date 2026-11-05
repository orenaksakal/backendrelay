#!/usr/bin/env bash
set -euo pipefail

readonly POSTGRES_VERSION=17.11
readonly WAL2JSON_TAG=wal2json_2_6

server_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$server_root"

for command in mise git make python3 cargo; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "required command is not installed: $command" >&2
        exit 1
    fi
done

echo "Ensuring PostgreSQL $POSTGRES_VERSION is available through mise..."
mise install "postgres@$POSTGRES_VERSION"
postgres_root=$(mise where "postgres@$POSTGRES_VERSION")
postgres_bin="$postgres_root/bin"

wal2json_library="$postgres_root/lib/wal2json.so"
if [[ ! -f "$wal2json_library" && ! -f "$postgres_root/lib/wal2json.dylib" ]]; then
    wal2json_cache="$postgres_root/instant-acceptance-cache/wal2json-$WAL2JSON_TAG"
    if [[ ! -d "$wal2json_cache/.git" ]]; then
        mkdir -p "$(dirname -- "$wal2json_cache")"
        git clone --depth 1 --branch "$WAL2JSON_TAG" \
            https://github.com/eulerto/wal2json.git "$wal2json_cache"
    fi
    echo "Building $WAL2JSON_TAG against PostgreSQL $POSTGRES_VERSION..."
    # Prebuilt PostgreSQL packages (for example conda builds that mise may pick)
    # report a compiler that isn't installed; fall back to the system cc.
    wal2json_make=(make -C "$wal2json_cache" PG_CONFIG="$postgres_bin/pg_config")
    pg_cc=$("$postgres_bin/pg_config" --cc | awk '{print $1}')
    if ! command -v "$pg_cc" >/dev/null 2>&1; then
        wal2json_make+=(CC=cc)
    fi
    "${wal2json_make[@]}"
    "${wal2json_make[@]}" install
fi

run_root=$(mktemp -d "${TMPDIR:-/tmp}/instant-pg17.XXXXXX")
data_root="$run_root/data"
postgres_log="$run_root/postgres.log"
acceptance_log="$server_root/target/postgres-acceptance.log"
full_workspace=false
mkdir -p "$server_root/target"
: >"$acceptance_log"

print_summary() {
    [[ -s "$acceptance_log" ]] || return 0
    awk '
        /^test result:/ {
            for (i = 1; i <= NF; i++) {
                if ($i == "passed;") passed += $(i - 1)
                if ($i == "failed;") failed += $(i - 1)
                if ($i == "ignored;") ignored += $(i - 1)
            }
        }
        END {
            printf "\nAcceptance test totals: %d passed, %d failed, %d ignored\n", passed, failed, ignored
        }
    ' "$acceptance_log"
    echo "Full test output: $acceptance_log"
}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [[ -f "$data_root/postmaster.pid" ]]; then
        "$postgres_bin/pg_ctl" -D "$data_root" -m immediate -w stop >/dev/null 2>&1 || true
    fi
    if ((status != 0)) && [[ -s "$postgres_log" ]]; then
        cp -- "$postgres_log" "$server_root/target/postgres-acceptance-postgres.log"
        echo "PostgreSQL failure log: $server_root/target/postgres-acceptance-postgres.log"
    fi
    case "$run_root" in
        */instant-pg17.*) rm -rf -- "$run_root" ;;
        *) echo "refusing to remove unexpected temporary path: $run_root" >&2 ;;
    esac
    if ((status == 0)) && [[ $full_workspace == true ]] &&
       grep -Eq 'TEST_DATABASE_URL is not set;.*skipped' "$acceptance_log"; then
        echo "acceptance tests silently skipped a required PostgreSQL contract" >&2
        status=1
    fi
    print_summary
    exit "$status"
}
trap cleanup EXIT INT TERM

port=$(python3 - <<'PY'
import socket

with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)

"$postgres_bin/initdb" \
    --pgdata="$data_root" \
    --username=instant_rust \
    --auth=trust \
    --encoding=UTF8 \
    --no-locale >/dev/null

printf '%s\n' \
    "listen_addresses = '127.0.0.1'" \
    "port = $port" \
    "unix_socket_directories = '$run_root'" \
    "wal_level = logical" \
    "max_replication_slots = 10" \
    "max_wal_senders = 10" \
    "output_plugin_libraries = 'wal2json'" \
    "timezone = 'UTC'" \
    "fsync = on" \
    "synchronous_commit = on" \
    "full_page_writes = on" \
    >>"$data_root/postgresql.conf"

"$postgres_bin/pg_ctl" -D "$data_root" -l "$postgres_log" -w start >/dev/null
"$postgres_bin/createdb" -h 127.0.0.1 -p "$port" -U instant_rust instant_rust
"$postgres_bin/createdb" -h 127.0.0.1 -p "$port" -U instant_rust instant_migrations

database_url="postgres://instant_rust@127.0.0.1:$port/instant_rust?sslmode=disable"
migration_url="postgres://instant_rust@127.0.0.1:$port/instant_migrations?sslmode=disable"

echo "Checking replication-slot monitoring against PostgreSQL catalog fixtures..."
PGHOST=127.0.0.1 PGPORT="$port" PGUSER=instant_rust PGDATABASE=instant_rust \
  MONITOR_TEST_PSQL="$postgres_bin/psql" \
  python3 "$server_root/../self-hosting/monitoring/test_slot_sql.py"

echo "Checking committed data and a logical slot across an immediate PostgreSQL restart..."
recovery_started=$(python3 -c 'import time; print(time.monotonic())')
"$postgres_bin/psql" "$database_url" -X --set ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE TABLE acceptance_restart_fixture (marker text PRIMARY KEY);
SELECT slot_name FROM pg_create_logical_replication_slot(
    'instant_acceptance_restart', 'wal2json'
);
INSERT INTO acceptance_restart_fixture VALUES ('committed-before-restart');
SQL
"$postgres_bin/pg_ctl" -D "$data_root" -m immediate -w stop >/dev/null
"$postgres_bin/pg_ctl" -D "$data_root" -l "$postgres_log" -w start >/dev/null

marker=$("$postgres_bin/psql" "$database_url" -AtX --set ON_ERROR_STOP=1 \
    -c "SELECT marker FROM acceptance_restart_fixture")
[[ "$marker" == committed-before-restart ]] || {
    echo "committed PostgreSQL fixture did not survive restart" >&2
    exit 1
}
slot_changes=$("$postgres_bin/psql" "$database_url" -AtX --set ON_ERROR_STOP=1 \
    -c "SELECT data FROM pg_logical_slot_get_changes('instant_acceptance_restart', NULL, NULL)")
[[ "$slot_changes" == *committed-before-restart* ]] || {
    echo "logical replication slot did not recover its pre-restart change" >&2
    exit 1
}
"$postgres_bin/psql" "$database_url" -X --set ON_ERROR_STOP=1 >/dev/null <<'SQL'
SELECT pg_drop_replication_slot('instant_acceptance_restart');
DROP TABLE acceptance_restart_fixture;
SQL

# Exercise a real dump and restore in this harness-owned cluster only.
"$postgres_bin/psql" "$database_url" -X --set ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE TABLE acceptance_restore_fixture (marker text PRIMARY KEY);
INSERT INTO acceptance_restore_fixture VALUES ('acknowledged-before-backup');
SQL
"$postgres_bin/pg_dump" "$database_url" --format=custom --file="$run_root/recovery.dump"
"$postgres_bin/createdb" -h 127.0.0.1 -p "$port" -U instant_rust instant_restore
restore_url="postgres://instant_rust@127.0.0.1:$port/instant_restore?sslmode=disable"
"$postgres_bin/pg_restore" --exit-on-error --no-owner --dbname="$restore_url" "$run_root/recovery.dump"
restored_marker=$("$postgres_bin/psql" "$restore_url" -AtX --set ON_ERROR_STOP=1 -c 'SELECT marker FROM acceptance_restore_fixture')
[[ "$restored_marker" == acknowledged-before-backup ]] || { echo 'Backup fixture was not restored' >&2; exit 1; }
"$postgres_bin/dropdb" -h 127.0.0.1 -p "$port" -U instant_rust instant_restore
"$postgres_bin/psql" "$database_url" -X --set ON_ERROR_STOP=1 -c 'DROP TABLE acceptance_restore_fixture' >/dev/null
python3 - "$recovery_started" <<'PYRECOVERY'
import json, sys, time
elapsed = time.monotonic() - float(sys.argv[1])
if elapsed > 30:
    raise SystemExit('Isolated storage recovery exceeded the 30-second acceptance target')
print(json.dumps({'storageRecovery': 'passed', 'elapsedSeconds': round(elapsed, 3),
    'targetSeconds': 30, 'acknowledgedFixtureRowsLost': 0, 'logicalSlotRecovered': True,
    'dumpRestore': 'passed', 'scope': 'disposable fixture; not production restore capacity'}))
PYRECOVERY
if [[ ${1:-} == --storage-recovery-only ]]; then exit 0; fi

profile_args=()
case ${INSTANT_ACCEPTANCE_PROFILE:-dev} in
    dev) ;;
    release) profile_args=(--release) ;;
    *) echo 'INSTANT_ACCEPTANCE_PROFILE must be dev or release' >&2; exit 2 ;;
esac

run_logged() {
    set +e
    "$@" 2>&1 | tee -a "$acceptance_log"
    command_status=${PIPESTATUS[0]}
    set -e
    return "$command_status"
}

echo "Running the CI migration lifecycle against a disposable database..."
export REQUIRE_TEST_DATABASE=1
run_logged env TEST_MIGRATION_DATABASE_URL="$migration_url" \
    cargo test --locked ${profile_args[@]+"${profile_args[@]}"} -p instant-server --test live_migrations
run_logged env TEST_MIGRATION_DATABASE_URL="$database_url" \
    cargo test --locked ${profile_args[@]+"${profile_args[@]}"} -p instant-server --test live_migrations

echo "Creating a non-superuser application role with REPLICATION..."
"$postgres_bin/psql" -h 127.0.0.1 -p "$port" -U instant_rust -d postgres -X -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE instant_app LOGIN REPLICATION;
CREATE DATABASE instant_app OWNER instant_app;
CREATE DATABASE instant_app_migrations OWNER instant_app;
SQL
app_url="postgres://instant_app@127.0.0.1:$port/instant_app?sslmode=disable"
app_migration_url="postgres://instant_app@127.0.0.1:$port/instant_app_migrations?sslmode=disable"

echo "Applying migrations as a non-superuser database owner..."
run_logged env DATABASE_URL="$app_url" \
    cargo run --locked ${profile_args[@]+"${profile_args[@]}"} -p instant-server -- migrate up
if [[ $("$postgres_bin/psql" "$app_url" -AtX -v ON_ERROR_STOP=1 \
    -c 'SELECT rolsuper FROM pg_roles WHERE rolname = current_user') != f ]]; then
    echo 'the application migration role must not be a superuser' >&2
    exit 1
fi

echo "Running the CI migration lifecycle as a non-superuser database owner..."
# The bounded-MCV test inspects pg_statistic_ext_data.stxdmcv directly, which
# PostgreSQL restricts to superusers and pg_read_all_stats. Migration 144
# itself is still covered by the non-superuser embedded_migrations test.
run_logged env TEST_MIGRATION_DATABASE_URL="$app_migration_url" \
    cargo test --locked ${profile_args[@]+"${profile_args[@]}"} -p instant-server --test live_migrations \
    -- --skip bounded_mcv_statistics_migration_preserves_rows_column_targets_and_rollback

echo "Recreating the disposable migration database for the workspace pass..."
"$postgres_bin/dropdb" -h 127.0.0.1 -p "$port" -U instant_rust instant_migrations
"$postgres_bin/createdb" -h 127.0.0.1 -p "$port" -U instant_rust instant_migrations

if (($#)); then
    echo "Running targeted Cargo acceptance arguments: $*"
    run_logged env RUST_TEST_THREADS=1 TEST_DATABASE_URL="$database_url" \
        TEST_MIGRATION_DATABASE_URL="$migration_url" \
        cargo test ${profile_args[@]+"${profile_args[@]}"} "$@"
else
    full_workspace=true
    echo "Running the full workspace with every TEST_DATABASE_URL live contract enabled..."
    run_logged env RUST_TEST_THREADS=1 TEST_DATABASE_URL="$database_url" \
        TEST_MIGRATION_DATABASE_URL="$migration_url" \
        cargo test --locked ${profile_args[@]+"${profile_args[@]}"} --workspace --no-fail-fast -- --nocapture
fi
