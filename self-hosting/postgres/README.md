# PostgreSQL image

The compose files build this image and tag it `backendrelay/postgres:17-local`. It starts from
the official `postgres:17.11-bookworm` image and adds the packages BackendRelay needs:

- `postgresql-17-wal2json` (`wal2json` 2.6), the required logical-decoding output plugin.
- `postgresql-17-pg-hint-plan` 1.7.1, for the planner hints the server emits. This is the PGDG
  package version; `self-hosting/scale-local/stack.sh` builds the `REL17_1_7_2` tag from source.
- `pgbackrest` 2.59.3, for the backups described in [`docs/self-hosting.md`](../../docs/self-hosting.md) and [`self-hosting/backup/README.md`](../backup/README.md). Its async spool volume is created root-owned, so the image's entrypoint makes `/var/spool/pgbackrest` writable by `postgres` before the stock entrypoint starts the server. Repository options come from `pgbackrest.env` at runtime.
- `initdb/10-backendrelay-role.sh`, which the stock entrypoint runs on a fresh data directory. It creates the unprivileged `INSTANT_DB_USER` role with `LOGIN` and `REPLICATION`, makes it the owner of the application database and the `public` schema, and leaves `postgres` as the only superuser. Backups keep using `postgres`. It also creates the `pg_stat_statements` extension so the monitoring exporter can read statement totals.

The three packages come from the apt.postgresql.org repository that the official image already
configures, and PGDG publishes them for amd64 and arm64.

## Bumping

Change the base image digest and the three package versions together, then rebuild and test the
stack. The versions are exact pins, so a package rebuild appears as an apt conflict until the pin
is updated. After pushing a digest, set `INSTANT_POSTGRES_IMAGE` to that digest in production.
