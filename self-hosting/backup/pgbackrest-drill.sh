#!/usr/bin/env bash
# Local pgBackRest drill against the BackendRelay PostgreSQL image:
# archive WAL, full + diff backup, point-in-time restore into a fresh data dir.
set -euo pipefail
image=${IMAGE:-backendrelay/postgres:17-local}
net=pgbr-drill-$$
repo=pgbr-drill-repo-$$
data=pgbr-drill-data-$$
restored=pgbr-drill-restored-$$
spool=pgbr-drill-spool-$$
cleanup() {
  [ -n "${KEEP:-}" ] || docker rm -f pgbr-src pgbr-restore >/dev/null 2>&1 || true
  [ -n "${KEEP:-}" ] || docker volume rm -f "$repo" "$data" "$restored" "$spool" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker network create "$net" >/dev/null
for v in "$repo" "$data" "$restored" "$spool"; do docker volume create "$v" >/dev/null; done
# The repo volume must be writable by the postgres user.
docker run --rm -v "$repo":/repo "$image" chown -R postgres:postgres /repo

pgbr_env=(
  -e PGBACKREST_STANZA=backendrelay
  -e PGBACKREST_PG1_PATH=/var/lib/postgresql/data
  -e PGBACKREST_PG1_USER=postgres
  -e PGBACKREST_REPO1_TYPE=posix
  -e PGBACKREST_REPO1_PATH=/repo
  -e PGBACKREST_REPO1_RETENTION_FULL_TYPE=time
  -e PGBACKREST_REPO1_RETENTION_FULL=14
  -e PGBACKREST_REPO1_CIPHER_TYPE=aes-256-cbc
  -e PGBACKREST_REPO1_CIPHER_PASS=drill-only-passphrase
  -e PGBACKREST_COMPRESS_TYPE=zst
  -e PGBACKREST_START_FAST=y
  -e PGBACKREST_ARCHIVE_ASYNC=y
  -e PGBACKREST_SPOOL_PATH=/var/spool/pgbackrest
  -e PGBACKREST_LOG_LEVEL_CONSOLE=warn
)

docker run -d --name pgbr-src --network "$net" "${pgbr_env[@]}" \
  -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=drill -e POSTGRES_DB=instant \
  -e INSTANT_DB_USER=instant -e INSTANT_DB_PASSWORD=drill \
  -v "$data":/var/lib/postgresql/data -v "$repo":/repo -v "$spool":/var/spool/pgbackrest "$image" \
  postgres -c wal_level=logical -c output_plugin_libraries=wal2json \
  -c archive_mode=on -c archive_timeout=60 \
  -c "archive_command=pgbackrest --stanza=backendrelay archive-push %p" >/dev/null
# The entrypoint's initdb runs a temporary server first; wait for the final one.
for _ in $(seq 1 90); do
  [ "$(docker logs pgbr-src 2>&1 | grep -c 'database system is ready to accept connections')" -ge 2 ] && break
  sleep 1
done
sleep 2
pgbr() { docker exec -u postgres pgbr-src pgbackrest "$@"; }
psql_src() { docker exec pgbr-src psql -U instant -d instant -Atc "$1"; }
# pg_switch_wal() needs the superuser; the application role cannot call it.
psql_admin() { docker exec pgbr-src psql -U postgres -d instant -Atc "$1"; }

pgbr stanza-create
pgbr check
psql_src "CREATE TABLE drill (id int primary key, note text); INSERT INTO drill SELECT g, 'before-full' FROM generate_series(1, 1000) g;"
pgbr --type=full backup
psql_src "INSERT INTO drill SELECT g, 'before-diff' FROM generate_series(1001, 2000) g;"
pgbr --type=diff backup
psql_src "INSERT INTO drill VALUES (5000, 'keep-me');"
psql_admin "SELECT pg_switch_wal();" >/dev/null
sleep 2
target=$(psql_src "SELECT now()::timestamptz(0) + interval '1 second';")
sleep 2
psql_src "DELETE FROM drill;"   # the mistake we recover from
psql_admin "SELECT pg_switch_wal();" >/dev/null
sleep 3
echo "rows at source after mistake: $(psql_src 'SELECT count(*) FROM drill')"
pgbr info | sed -n '1,20p'

docker rm -f pgbr-src >/dev/null
docker run --rm -u postgres "${pgbr_env[@]}" -v "$restored":/var/lib/postgresql/data -v "$repo":/repo "$image" \
  sh -c "pgbackrest --type=time '--target=$target' --target-action=promote restore"
docker run -d --name pgbr-restore "${pgbr_env[@]}" -v "$restored":/var/lib/postgresql/data -v "$repo":/repo -v "$spool":/var/spool/pgbackrest "$image" \
  postgres -c wal_level=logical -c output_plugin_libraries=wal2json -c archive_mode=off >/dev/null
for _ in $(seq 1 90); do
  docker exec pgbr-restore psql -U instant -d instant -Atc "SELECT NOT pg_is_in_recovery()" 2>/dev/null | grep -q t && break
  sleep 1
done
count=$(docker exec pgbr-restore psql -U instant -d instant -Atc "SELECT count(*) FROM drill")
keep=$(docker exec pgbr-restore psql -U instant -d instant -Atc "SELECT count(*) FROM drill WHERE note = 'keep-me'")
echo "restored to $target: rows=$count keep-me=$keep"
test "$count" = 2001 && test "$keep" = 1
echo "DRILL PASSED"
