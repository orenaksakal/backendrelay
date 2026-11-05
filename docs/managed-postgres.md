# Using a managed PostgreSQL service

BackendRelay can use a managed PostgreSQL service instead of the bundled database
container, as long as the service meets the requirements below. This page lists
those requirements and the settings needed for PlanetScale for Postgres.

## Requirements

| Requirement | Why |
|---|---|
| PostgreSQL 17 | Tested with 17.11. |
| Logical decoding with the **`wal2json`** output plugin | Realtime updates come from a logical replication slot that the server creates with `wal2json` ([`replication_slot.rs`](../server-rs/crates/instant-pg/src/replication_slot.rs)). Other plugins such as `pgoutput` are rejected. |
| `wal_level = logical`, free replication slots and WAL senders | The server holds one logical slot and one replication connection. |
| A role that can create logical replication slots (`REPLICATION`) | The server creates its own slot at startup. |
| Extensions `btree_gist`, `pgcrypto`, `pg_trgm` | Created by the migrations in [`migrations/`](../server-rs/crates/instant-server/migrations/). |
| Direct connections (not a transaction-mode pooler) | The server uses the replication protocol and per-session prepared statements. |
| Enough `max_connections` | The server opens `CONNECTION_POOL_SIZE` pooled connections plus about 36 dedicated connections for background workers, policies, replication and `LISTEN`. Allow `CONNECTION_POOL_SIZE` + 41 or more; the bundled Compose files set `200`. |
| Optional: `pg_hint_plan`, `pg_stat_statements` | Query hints are emitted as comments and ignored without `pg_hint_plan`; `pg_stat_statements` helps diagnose load. Add both to `shared_preload_libraries` and create the `pg_stat_statements` extension once as superuser. |

Run `instant-server migrate up` against the new database before starting the
server; serving verifies migrations but never applies them.

## Settings checklist

Use the same values as the self-managed Compose stack. On a managed service set
these as cluster parameters (the provider may call them *database parameters* or
*fleet parameters*) and restart the primary if the provider requires it for
`shared_preload_libraries`.

| Setting | Value | Why |
|---|---|---|
| `shared_buffers` | About 25% of the RAM PostgreSQL can count on; the Compose production default is `6GB` | PostgreSQL's own page cache. Leave room for the server, the OS, and monitoring on the same host. |
| `effective_cache_size` | About 50–60% of that RAM; the Compose production default is `16GB` | Tells the planner how much data is likely cached by PostgreSQL and the OS. It is not an allocation. |
| `max_connections` | `200`, and never below `instances × (CONNECTION_POOL_SIZE + 41)` | The server opens a main pool plus about 36 dedicated connections per instance; PgBouncer is not an option. |
| `shared_preload_libraries` | `pg_hint_plan,pg_stat_statements` | Loads the planner-hint and statement-statistics extensions. |
| `log_min_duration_statement` | `500ms` | Logs slow statements for investigation without logging every query. |
| `triples` autovacuum | `autovacuum_vacuum_scale_factor=0.02`, `autovacuum_analyze_scale_factor=0.01` | Migration `151_triples_autovacuum` applies these table overrides; no cluster change is needed. |
| `triples_app_id` index | Dropped by migration `150_drop_triples_app_id` | It duplicated the leading column of `triples`' primary key and only added write cost. |

Verify after applying migrations:

```sql
SELECT reloptions FROM pg_class WHERE oid = 'public.triples'::regclass;
SELECT to_regclass('public.triples_app_id'); -- must be NULL
```

## Database role

Use a dedicated login role that is **not** a superuser. It needs `LOGIN` and
`REPLICATION`, and it must own the application database (or at least the
schemas the migrations write to) so the first `migrate up` can create
`pgcrypto`, `btree_gist`, and `pg_trgm`. Those extensions are trusted in
PostgreSQL 17, so their owner can create them without superuser rights. Do not
grant `SUPERUSER`, `CREATEROLE`, or `CREATEDB`. The bundled Compose stack
creates this role from `INSTANT_DB_USER` / `INSTANT_DB_PASSWORD` on first boot
and keeps `postgres` for backups and administration only.

Verify the application connection with
`SELECT rolsuper FROM pg_roles WHERE rolname = current_user;`, which must
return `f`.

Keep the database in the same region as the server. Each user operation issues
several SQL statements in sequence, so cross-region latency directly adds to
request latency and to how long pooled connections stay busy.

## Known limitation: replication slots and failover

The server currently creates its logical replication slot without the PostgreSQL
17 `failover` flag. On a service that promotes a standby, during an automatic
failover, a planned switchover or a resize that replaces the primary, a slot
without that flag does not move to the new primary, and realtime delivery stops
until the slot is recreated. Single-node self-hosted PostgreSQL is not affected.

Until this is addressed, treat failovers and resizes on managed HA services as
maintenance events, and check realtime delivery afterwards.

## PlanetScale for Postgres

PlanetScale for Postgres supports `wal2json` 2.6, the required extensions, and
roles with the `REPLICATION` attribute.

1. **Create a role with replication** (dashboard: Settings → Roles → *WITH
   REPLICATION*, or `CREATE USER ... WITH REPLICATION`). When connecting, append
   the branch ID to the user name (`user.<branch-id>`).
2. **Connect directly on port 5432**, not through PgBouncer.
3. **Set cluster parameters** (Clusters → Parameters), following the
   [settings checklist](#settings-checklist):
   - `wal_level = logical`
   - `max_replication_slots` and `max_wal_senders` per PlanetScale's formula for
     your replica count, plus one slot for BackendRelay
   - `max_slot_wal_keep_size` of at least a few GB, so a disconnected server cannot
     fill the disk with retained WAL
   - `shared_buffers` at about 25% and `effective_cache_size` at about 50–60% of
     the cluster's RAM
   - `max_connections = 200`, and never below `CONNECTION_POOL_SIZE` + 41 per
     server instance (the server keeps about 36 dedicated connections besides its
     pool; PlanetScale's defaults are conservative on small clusters)
   - `shared_preload_libraries = pg_hint_plan,pg_stat_statements` (requires a
     restart)
   - `log_min_duration_statement = 500ms`
   - for HA clusters, `sync_replication_slots = on` and `hot_standby_feedback = on`
     (needed once slots are created with the failover flag)
4. **Enable the extensions** as a superuser once:
   `CREATE EXTENSION IF NOT EXISTS pg_hint_plan;` and
   `CREATE EXTENSION IF NOT EXISTS pg_stat_statements;`. Or enable them in the
   dashboard if PlanetScale exposes the toggle.
5. **Size the cluster** using [performance.md](performance.md#sizing). For a new
   deployment with a handful of concurrent users, a single-node cluster with
   ¼ vCPU and 2 GiB of memory, `CONNECTION_POOL_SIZE=16` on the server and
   `max_connections` of about 60 is plenty; resize later. Because a resize replaces the primary, see the
   [known limitation](#known-limitation-replication-slots-and-failover).

References: [PlanetScale Postgres extensions](https://planetscale.com/docs/postgres/extensions),
[logical replication setup](https://planetscale.com/docs/postgres/integrations/logical-cdc),
[cluster parameters](https://planetscale.com/docs/postgres/cluster-configuration/parameters),
[single-node clusters](https://planetscale.com/docs/postgres/cluster-configuration/single-node).

## Other providers

Before choosing a provider, confirm that it allows `wal2json` logical replication
slots from a user role and direct (non-pooled) connections. Providers that only
offer `pgoutput`, or that block slot creation, cannot run BackendRelay's realtime
features.
