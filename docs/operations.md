# Operations

Run the commands below from `self-hosting/` after defining these helpers once in
the shell. They use the production Caddy file and the monitoring overlay.

```sh
dc()   { docker compose -f docker-compose.with-caddy.yml -f docker-compose.monitoring.yml "$@"; }
sql()  { dc exec -T -u postgres postgres psql -U postgres -d instant "$@"; }
prom() { dc exec -T prometheus promtool query instant http://localhost:9090 "$1"; }
```

`sql` assumes `POSTGRES_DB=instant`; use your database name if it differs.
`prom` reads the same Prometheus that evaluates the alert rules. Grafana
Explore (`{container=~".*server.*"}`) searches the Loki copy of container logs.

## Finding problems

Start with the Grafana dashboard (`http://127.0.0.1:3001`, **BackendRelay single
VPS**). The **Per app** row separates realtime sessions, operations, database
time, errors, writes, triples bytes and webhook queue depth by the bounded `app`
label. Grafana is loopback-only; reach it through `tailscale serve` or an SSH
tunnel.

### Which app is hurting us

The `app` label is a slug from the `app-metric-slugs` flag, or the first eight
characters of the app UUID:

```sh
dc exec -T server /usr/local/bin/instant-server flags get app-metric-slugs
```

Use the dashboard row, or these Prometheus queries:

```promql
topk(10, sum by (app) (rate(backendrelay_app_ops_total[5m])))
topk(10, sum by (app) (rate(backendrelay_app_db_seconds_total[5m])))
topk(10, backendrelay_app_writes_per_minute)
topk(10, sum by (app) (rate(backendrelay_app_errors_total[5m])))
topk(10, backendrelay_app_webhook_events_failed)
```

Map a prefix back to an app:

```sh
sql -c "SELECT id, title, status FROM apps WHERE id::text LIKE '<app-label>%';"
```

For a global view of expensive statements, use `pg_stat_statements`. It is
preloaded and the extension is created by the PostgreSQL image:

```sh
sql -c "SELECT calls,
               round(total_exec_time::numeric, 1) AS exec_ms,
               round(mean_exec_time::numeric, 2) AS mean_ms,
               rows,
               left(regexp_replace(query, '\s+', ' ', 'g'), 160) AS query
          FROM pg_stat_statements
         ORDER BY total_exec_time DESC
         LIMIT 10;"
```

Statements are not attributed to an app; use the per-app database-time metric
above to pick the tenant, then the statement list to see what it runs.

### Reproduce a user bug

The dashboard **Sandbox** runs queries and transactions as one user with the
app's rules applied. Open it directly:

```text
/dash?app=<app-id>&t=sandbox&s=main
```

Set **auth.email** to the user's address to run as that user. Leave it empty to
run as a guest. The Sandbox has no token field; when the bug depends on a
refresh token, use the CLI or the admin API, which send the same
`as-email`, `as-token` and `as-guest` headers:

```sh
instant-cli query --app <app-id> --as-token <refresh-token> '{"posts":{}}'
instant-cli query --app <app-id> --as-email user@example.com '{"posts":{}}'
instant-cli query --app <app-id> --as-guest '{"posts":{}}'
```

Superadmin tokens also have two read-only operator endpoints:

```sh
curl -H "Authorization: Bearer $TOKEN" "$API/superadmin/apps/<app-id>/runtime"
curl -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"query":{"posts":{}},"as":{"email":"user@example.com"}}' \
  "$API/superadmin/apps/<app-id>/explain"
```

`runtime` reports this server process's sessions, subscriptions by query hash and
the oldest queued refresh wait. `explain` returns PostgreSQL planning and
execution time plus the indexes used; it never returns app rows.

Keep **Use saved app rules** checked to match production. Turn it off only to
prove whether a rule is the cause. A reproduced request also appears in the JSON
log with its `request_id`; search Loki for that id:

```text
{container=~".*server.*"} |= "<request-id>"
```

## App controls

Use the narrowest control that fixes the problem: a throttled app keeps serving,
`read-only` keeps reads, and `disabled` stops both reads and writes. Status
changes are pushed to live sessions as `app-status-changed` and clear the app's
caches.

### Throttle realtime refreshes

`refresh-throttled-apps` is a JSON array of app IDs; `refresh-throttle-ms`
defaults to 1000:

```sh
dc exec -T server /usr/local/bin/instant-server flags set refresh-throttled-apps '["<app-id>"]'
dc exec -T server /usr/local/bin/instant-server flags set refresh-throttle-ms 2000
```

Remove the app from the array (or set `[]`) to restore normal refresh rate.
Throttling increases realtime latency and does not reject writes.

### Rate-limit an app

The existing hard limiter is the legacy config-app `rate-limited-apps`
collection checked by `app_rate_limit.rs`. It is **not** a `flags` key (see
[configuration.md](configuration.md#config-app-flags)), and this build has no
CLI writer for it: do not run `instant-server flags set rate-limited-apps ...`,
which would write a flag the rate limiter never reads. Use the throttle flags
above for routine control; reserve the hard limit for emergencies.

Inspect the entries:

```sh
sql -c "SELECT DISTINCT t.value #>> '{}' AS app_id
          FROM triples t
          JOIN attrs a ON a.id = t.attr_id
         WHERE t.app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'
           AND a.etype = 'rate-limited-apps'
           AND a.label = 'appId'
         ORDER BY 1;"
```

A rate-limited app gets HTTP 429; the message names the deployment operator and
says to contact them. Lift an existing entry with:

```sh
sql -c "DELETE FROM triples
         WHERE app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'
           AND attr_id = (SELECT id FROM attrs
                           WHERE app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'
                             AND etype = 'rate-limited-apps'
                             AND label = 'appId')
           AND value #>> '{}' = '<app-id>';"
```

Adding an entry requires the config app's native transaction path, which the
current dashboard operator pages do not expose. Prefer disabling the app or
lowering its session and refresh limits until that path ships.

### Disable or restore one app

The dashboard's **Admin** tab toggles **Read-only mode** through
`POST /dash/apps/<app-id>/status`. The same endpoint accepts `disabled`; these
SQL statements are equivalent and take effect on the next request:

```sh
sql -c "UPDATE apps SET status = 'read-only' WHERE id = '<app-id>';"
sql -c "UPDATE apps SET status = 'disabled'  WHERE id = '<app-id>';"
sql -c "UPDATE apps SET status = 'active'    WHERE id = '<app-id>';"
```

`read-only` returns `This app is in read-only mode.` for writes; `disabled`
returns `This app is currently disabled.` for reads and writes. Restore the app
to `active` when the incident is over.

### Restore everything

This reactivates every app that is not hard-deleted (it skips rows with
`deletion_marked_at` set):

```sh
sql -c "UPDATE apps SET status = 'active'
         WHERE status <> 'active' AND deletion_marked_at IS NULL;"
```

Check the result before declaring recovery:

```sh
sql -c "SELECT id, title, status FROM apps ORDER BY created_at;"
```

## Alert playbooks

Every alert in [`self-hosting/monitoring/alerts.yml`](../self-hosting/monitoring/alerts.yml)
has a section below. The `dc`, `sql` and `prom` helpers are defined at the top
of this page.

### Watchdog

**What it means.** Alertmanager evaluated its rules and sent this heartbeat to
the external dead-man service. It never pages by itself; the external check
pages when the heartbeat stops.

**First three commands.**
1. `dc ps alertmanager prometheus`
2. `dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 alert query`
3. `dc logs --since 15m alertmanager`

**Likely causes.** Alertmanager is down, restarted, or cannot load its config;
the `external-deadman` route or webhook file is missing; the host cannot reach
the dead-man service.

**Fix.** Start Alertmanager (`dc up -d alertmanager`), run
`dc exec -T alertmanager amtool check-config /etc/alertmanager/alertmanager.yml`,
and confirm the `Watchdog` route in `alertmanager.yml` still points at
`external-deadman`. Test the external check from outside the host.

**Escalate when.** The heartbeat has been absent for more than five minutes and
Pushover is also silent; the paging path itself is down.

### ServiceUnavailable

**What it means.** A required Prometheus target has answered `up == 0` for two
minutes, so a scrape or the service behind it is gone.

**First three commands.**
1. `prom 'up == 0'`
2. `dc ps`
3. `dc logs --since 15m <service>`

**Likely causes.** Container crashed or was OOM-killed, cannot start after a
config or image change, port or network mismatch, or the exporter/canary target
was removed.

**Fix.** Restart the named service (`dc up -d <service>`), correct the compose
or scrape configuration, and confirm `prom 'up == 0'` is empty. If it is
`server` or `postgres`, use the DatabaseUnavailable or RequestErrors playbook.

**Escalate when.** The server or PostgreSQL stays down after one restart, or the
canary is the only unavailable target and the API is otherwise healthy.

### RequestErrors

**What it means.** More than one percent of HTTP responses have been 5xx for
five minutes.

**First three commands.**
1. `prom 'sum by (surface) (rate(backendrelay_http_requests_total{status_class="5xx"}[5m]))'`
2. `dc logs --since 30m server`
3. `dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 alert query`

**Likely causes.** Database or pool failures, a panic, a bad deploy, storage
errors, or one app sending invalid work that reaches an internal error.

**Fix.** Take a `trace_id` or `request_id` from the log or the client error and
search Loki for it. If the rate rose with a deploy, roll back; otherwise fix the
named dependency. Check `ProcessPanics` and `DatabaseUnavailable` for a common
cause.

**Escalate when.** Errors exceed five percent, a write path returns 5xx, or the
cause is data loss or corruption.

### DatabaseUnavailable

**What it means.** `pg_up` is zero or the PostgreSQL exporter is not being
scraped for two minutes.

**First three commands.**
1. `dc ps postgres postgres-exporter`
2. `dc exec -T postgres pg_isready --username postgres --dbname instant`
3. `dc logs --since 15m postgres`

**Likely causes.** PostgreSQL stopped, the host is out of disk, the container
was OOM-killed, or the exporter DSN or password is wrong.

**Fix.** Free disk first, then start PostgreSQL (`dc up -d postgres`). A single
self-managed primary has no provider failover; if the data directory is damaged,
restore it with pgBackRest (see [Database](#database)). Confirm the server pools
reconnect and `/health/system` returns `{"wal":"ok"}`.

**Escalate when.** PostgreSQL does not start after one attempt, or the data
directory needs a restore. This is an immediate page.

### DatabaseMetricsFailed

**What it means.** `pg_exporter_last_scrape_error` is non-zero, or the required
`backendrelay_wal_retained_bytes` sample is missing for two minutes.

**First three commands.**
1. `dc logs --since 15m postgres-exporter`
2. `prom 'pg_exporter_last_scrape_error'`
3. `sql -c "SELECT extname FROM pg_extension WHERE extname = 'pg_stat_statements';"`

**Likely causes.** The exporter role or password changed, `pg_stat_statements`
is missing, `POSTGRES_MONITOR_QUERIES_FILE` points at an unreadable or invalid
file, or the exporter cannot reach PostgreSQL.

**Fix.** Recreate the exporter role from
`monitoring/postgres-role.sql`, run `CREATE EXTENSION IF NOT EXISTS
pg_stat_statements;` as superuser, fix the queries-file mount, then restart
`postgres-exporter`. The DatabaseUnavailable rule still covers the database
itself; this rule means you are blind to capacity and slots.

**Escalate when.** Metrics stay missing for more than 30 minutes or WAL and slot
health cannot be observed.

### PostgresConnectionsHigh

**What it means.** Connected backends exceed 80 percent of `max_connections`
for ten minutes.

**First three commands.**
1. `sql -c "SELECT (SELECT count(*) FROM pg_stat_activity) AS used, current_setting('max_connections') AS max;"`
2. `sql -c "SELECT usename, state, count(*) FROM pg_stat_activity GROUP BY 1, 2 ORDER BY 3 DESC;"`
3. `prom 'backendrelay_pool_waiting'`

**Likely causes.** `CONNECTION_POOL_SIZE` is too large for the number of server
processes, a BYOP app opens many connections, a pool leaks, or long
transactions and idle sessions hold slots.

**Fix.** Terminate idle or abandoned sessions, fix the long transaction, and
lower `CONNECTION_POOL_SIZE`. Raise `max_connections` only after checking
memory and PostgreSQL's per-connection overhead; the connection count already
includes monitoring and backup users.

**Escalate when.** Connections reach 100 percent or the server cannot acquire a
connection.

### PostgresLongTransaction

**What it means.** A client transaction has been open for more than five
minutes for five minutes.

**First three commands.**
1. `sql -c "SELECT pid, usename, state, now() - xact_start AS age, left(query, 120) FROM pg_stat_activity WHERE xact_start IS NOT NULL ORDER BY age DESC LIMIT 5;"`
2. `sql -c "SELECT pg_cancel_backend(<pid>);"`
3. `dc logs --since 30m server`

**Likely causes.** A slow or unindexed query, a lock wait, an interactive pool
query that bypassed the timeout, or a BYOP transaction that never commits.

**Fix.** Cancel the backend first; use `pg_terminate_backend(<pid>)` only when
cancel does not work. Find the statement in `pg_stat_statements` or the server
log, then add the index or fix the client. The default interactive
`statement_timeout` is 15 seconds; a five-minute transaction is a bug or a
maintenance action, not normal load.

**Escalate when.** Long transactions recur, block realtime refreshes, or cause
WAL retention to grow.

### PostgresIdleInTransaction

**What it means.** A session has been idle inside an open transaction for more
than a minute.

**First three commands.**
1. `sql -c "SELECT pid, usename, now() - state_change AS idle_age, left(query, 120) FROM pg_stat_activity WHERE state = 'idle in transaction' ORDER BY idle_age DESC;"`
2. `sql -c "SELECT pg_terminate_backend(<pid>);"`
3. `prom 'backendrelay_pool_waiting'`

**Likely causes.** A client or dashboard handler forgot to commit or roll back,
or a pool release path is broken.

**Fix.** Terminate the session. `INSTANT_PG_IDLE_IN_TX_TIMEOUT_MS` (default
60 s) should already end it; if it returns, find the caller that opened the
transaction and fix the missing commit. Idle transactions hold snapshots and
prevent vacuum cleanup.

**Escalate when.** It returns repeatedly or holds locks that block writes.

### PostgresDeadlocks

**What it means.** PostgreSQL recorded at least one deadlock in the last ten
minutes.

**First three commands.**
1. `sql -c "SELECT datname, deadlocks FROM pg_stat_database ORDER BY deadlocks DESC;"`
2. `dc logs --since 30m postgres`
3. `sql -c "SELECT pid, state, wait_event_type, left(query, 120) FROM pg_stat_activity WHERE wait_event_type = 'Lock';"`

**Likely causes.** Two transactions update the same rows in different orders,
long transactions widen the window, or a schema or permission change races with
application writes.

**Fix.** The server retries some transactions, so one deadlock is usually
survivable. Read the PostgreSQL log for the two statements, then make the
writers take locks in a consistent order or shorten the transactions. Do not
disable deadlock detection.

**Escalate when.** Deadlocks repeat, surface as client 5xx, or accompany a
write outage.

### TriplesDeadTuplesHigh

**What it means.** Dead tuples are more than 20 percent of `triples` for fifteen
minutes.

**First three commands.**
1. `sql -c "SELECT relname, n_live_tup, n_dead_tup, last_autovacuum, last_vacuum FROM pg_stat_user_tables WHERE relname = 'triples';"`
2. `sql -c "VACUUM (ANALYZE) triples;"`
3. `dc logs --since 1h postgres`

**Likely causes.** High update or delete churn, autovacuum starved by long
transactions, or autovacuum workers busy on other tables.

**Fix.** Run the manual vacuum above; it is safe while serving. Migration 151
sets the `triples` scale factors to 2 percent vacuum and 1 percent analyze.
Check `autovacuum_max_workers` and `autovacuum_naptime` if it returns, and look
for a long transaction holding an old snapshot.

**Escalate when.** Vacuum does not reduce dead tuples, or table and index bloat
keeps growing.

### DatabaseSizeGrowth

**What it means.** A database grew more than 20 percent in the last day for ten
minutes.

**First three commands.**
1. `sql -c "SELECT datname, pg_size_pretty(pg_database_size(datname)) FROM pg_database ORDER BY pg_database_size(datname) DESC;"`
2. `sql -c "SELECT relname, pg_size_pretty(pg_total_relation_size(relid)) FROM pg_catalog.pg_statio_user_tables ORDER BY pg_total_relation_size(relid) DESC LIMIT 10;"`
3. `prom 'topk(10, backendrelay_app_writes_per_minute)'`

**Likely causes.** A bulk import or backfill, `triples` or `transactions`
growth, history or webhook partitions not being truncated, or bloat.

**Fix.** Identify the app from the write metric and ask the owner whether the
import is expected. Check `INSTANT_HISTORY_RETENTION_DAYS` and
`INSTANT_TRANSACTION_RETENTION_DAYS`, and see [Data retention](#data-retention).
Vacuum or reindex only after confirming bloat.

**Escalate when.** Growth exceeds 50 percent in a day, the disk headroom rule
also fires, or retention jobs are not running.

### RequestLatency

**What it means.** HTTP response-header p95 exceeds 750 ms for ten minutes on a
non-health surface.

**First three commands.**
1. `prom 'histogram_quantile(0.95, sum by (le, surface) (rate(backendrelay_http_request_duration_seconds_bucket{surface!="health"}[5m])))'`
2. `prom 'topk(10, sum by (app) (rate(backendrelay_app_db_seconds_total[5m])))'`
3. `dc logs --since 30m server`

**Likely causes.** Database latency, lock waits, cache misses, CPU saturation,
or one app running expensive queries. The metric measures time to response
headers, so it includes query planning and execution.

**Fix.** Find the surface and app, then use [Which app is hurting
us](#which-app-is-hurting-us) and the `pg_stat_statements` list. Apply the app
controls before scaling the host; check `PoolPressure` and
`RealtimeRefreshBacklog` for a shared cause.

**Escalate when.** p95 exceeds two seconds, errors rise with it, or the database
is the bottleneck and needs a resize.

### PoolPressure

**What it means.** More than ten database acquisitions have been waiting for
five minutes.

**First three commands.**
1. `prom 'backendrelay_pool_waiting'`
2. `sql -c "SELECT state, count(*) FROM pg_stat_activity GROUP BY 1 ORDER BY 2 DESC;"`
3. `dc logs --since 15m server`

**Likely causes.** Pool too small for realtime load, long transactions or slow
queries holding leases, too many sessions, or PostgreSQL disabled by CPU or
locks.

**Fix.** Reduce the work holding connections first. Raise
`CONNECTION_POOL_SIZE` only within the `PostgresConnectionsHigh` headroom, then
restart the server. Throttle the app that dominates database time.

**Escalate when.** Waits turn into timeouts or user-visible latency, or
PostgreSQL is out of connections.

### ServerMemoryHigh

**What it means.** The server's resident set is above 80 percent of its
container memory limit for ten minutes.

**First three commands.**
1. `dc stats --no-stream server`
2. `prom 'backendrelay_process_resident_memory_bytes / clamp_min(backendrelay_process_memory_limit_bytes, 1)'`
3. `prom 'backendrelay_result_cache_bytes / clamp_min(backendrelay_result_cache_max_bytes, 1)'`

**Likely causes.** Per-app result caches approaching their budget, many live
sessions, a leak, or a memory limit set too low for the configured cache budget.

**Fix.** Lower `INSTANT_RESULT_CACHE_MAX_BYTES` or the `datalog-cache-max-weight`
flag, reduce sessions for the hot app, and restart if the process is already
unstable. The result cache evicts under its own budget; this alert means the
process, not one cache, is near the cgroup limit.

**Escalate when.** The container is OOM-killed, restarts, or memory keeps rising
after a cache-limit change.

### AdmissionPressure

**What it means.** Realtime query or transaction admission is rejecting more
than one operation per second for five minutes.

**First three commands.**
1. `prom 'sum(rate(backendrelay_query_admission_rejections_total[5m]) + rate(backendrelay_transaction_admission_rejections_total[5m]))'`
2. `prom 'topk(10, sum by (app) (rate(backendrelay_app_ops_total[5m])))'`
3. `dc logs --since 15m server`

**Likely causes.** A traffic burst, one app monopolising the interactive pool,
or admission limits below the deployment's normal load.

**Fix.** Throttle the app with `refresh-throttled-apps`, reduce its session cap,
or scale the server. Do not simply raise admission limits while PostgreSQL is
already the bottleneck; check `PoolPressure` and `RequestLatency` first.

**Escalate when.** Rejections last more than 15 minutes or clients cannot
establish sessions.

### RealtimeRefreshBacklog

**What it means.** A refresh consumer has more than 50 waiting refreshes for
five minutes.

**First three commands.**
1. `prom 'max by (consumer) (backendrelay_refresh_waiting)'`
2. `prom 'topk(10, sum by (app) (rate(backendrelay_app_ops_total[5m])))'`
3. `dc logs --since 15m server`

**Likely causes.** Slow refresh queries, a hot app with many subscriptions, WAL
apply lag, or the refresh pool starved by interactive work.

**Fix.** Throttle or disable the app that dominates, then check database latency
and `RealtimeReceiveWait`. Refreshes are per session; reducing a runaway app's
subscriptions also reduces the queue.

**Escalate when.** Waiters exceed the admission limit and refreshes start
failing, or data visible to users is stale.

### RealtimeRefreshRejected

**What it means.** Refresh admission rejected work in the last ten minutes.

**First three commands.**
1. `prom 'sum by (consumer) (increase(backendrelay_refresh_admission_rejections_total[10m]))'`
2. `prom 'max by (consumer) (backendrelay_refresh_waiting)'`
3. `dc logs --since 15m server`

**Likely causes.** The global refresh pool or a per-app waiter cap
(`INSTANT_REFRESH_WAITERS_PER_APP`, default 64) is full.

**Fix.** Find the app with the backlog, throttle it, and reduce its session or
subscription count. Raise the waiter cap only if PostgreSQL and the refresh pool
have headroom; otherwise it moves the queue rather than draining it.

**Escalate when.** A tenant sees missing realtime updates or the same app is
rejected repeatedly.

### RealtimeReceiveWait

**What it means.** The oldest realtime receive batch has waited more than five
seconds for five minutes.

**First three commands.**
1. `prom 'max by (consumer) (backendrelay_receive_oldest_wait_ms)'`
2. `prom 'backendrelay_wal_retained_bytes'`
3. `dc logs --since 15m server`

**Likely causes.** The WAL sink is slow or blocked on PostgreSQL, a large
transaction delays a window, the durable sink is behind, or the process is CPU
starved.

**Fix.** Check `RealtimeRefreshBacklog`, `PoolPressure` and the database. If the
WAL consumer keeps up but one session's refresh is slow, the
`INSTANT_WAL_SINK_REFRESH_DEADLINE_MS` path should fail that refresh instead of
the window. Restart the server only after capturing logs, because the restart
replays from the slot.

**Escalate when.** Wait stays above 30 seconds, WAL retention rises, or the
canary propagation alert also fires.

### WebsocketExitsHigh

**What it means.** WebSocket loops are exiting abnormally faster than 0.5 per
second for five minutes, excluding normal peer close and inbound EOF.

**First three commands.**
1. `prom 'sum by (reason) (rate(backendrelay_websocket_exits_total{reason!~"peer-close|inbound-eof"}[5m]))'`
2. `dc logs --since 15m server`
3. `dc ps server`

**Likely causes.** A deploy or restart (`service-restart`), outbound send
timeouts, idle connections the client does not revive, or a network path
dropping frames. Client-side reconnects usually explain moderate rates.

**Fix.** Work from the `reason` label. `service-restart` is expected during a
drain; `outbound-send-timeout` and `idle-unrevived` point at client health or
the outbound queue. Check `RealtimeRefreshBacklog` and server CPU before
changing limits.

**Escalate when.** Exits are dominated by server-side reasons, the server
restarts unexpectedly, or users cannot stay connected.

### CacheBudgetPressure

**What it means.** The process-wide result cache is above 90 percent of its byte
budget for ten minutes.

**First three commands.**
1. `prom 'backendrelay_result_cache_bytes / clamp_min(backendrelay_result_cache_max_bytes, 1)'`
2. `prom 'backendrelay_result_cache_max_bytes'`
3. `dc logs --since 15m server`

**Likely causes.** Many large result sets, a high per-app
`datalog-cache-max-weight`, or a process budget that is low for the workload.
Shared root frames are charged to the same budget. Eviction is expected at the
budget; sustained pressure means churn.

**Fix.** Lower the per-app weight with `flags set datalog-cache-max-weight
<bytes>` or raise `INSTANT_RESULT_CACHE_MAX_BYTES` if `ServerMemoryHigh` has
headroom. Watch request latency: eviction costs a re-query.

**Escalate when.** Memory pressure accompanies it or latency rises from cache
churn.

### ProcessPanics

**What it means.** The server recorded a panic in the last ten minutes.

**First three commands.**
1. `prom 'increase(backendrelay_panics_total[10m])'`
2. `dc logs --since 30m server`
3. `dc ps server`

**Likely causes.** A code bug, a poisoned lock, an invalid assumption on
untrusted input, or an allocation failure.

**Fix.** Capture the JSON panic line with its location and thread, and the
Bugsink event if configured. Restart only if the process is unhealthy; a panic
in one task may not require a restart. Roll back if it started with a release,
and file the location in the tracker.

**Escalate when.** It happens immediately. Repeated panics or a crash loop are
an outage.

### CleanupBacklog

**What it means.** The oldest realtime cleanup retry is more than two minutes
old for five minutes.

**First three commands.**
1. `prom 'backendrelay_cleanup_oldest_retry_age_seconds'`
2. `dc logs --since 15m server`
3. `sql -c "SELECT state, count(*) FROM pg_stat_activity GROUP BY 1 ORDER BY 2 DESC;"`

**Likely causes.** Cleanup writes are failing on the database, a lock or long
transaction blocks them, or the retry queue is stuck behind one bad entry.

**Fix.** Fix the database error shown in the log; cleanup retries drain once
writes succeed. If it is stuck after the database recovers, restart the server
so the retry queue is rebuilt. Room and presence state is eventually consistent,
but stale rows can keep sessions alive longer than expected.

**Escalate when.** Retries are older than ten minutes or rooms and presence
stop updating.

### WalRetentionHigh

**What it means.** Logical replication slots retain more than 1 GiB of WAL for
ten minutes.

**First three commands.**
1. `sql -c "SELECT slot_name, active, wal_status, pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS retained FROM pg_replication_slots WHERE slot_name = 'aggregator' OR slot_name LIKE 'invalidator\\_%' ESCAPE '\\' ORDER BY pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) DESC;"`
2. `prom 'backendrelay_wal_retained_bytes'`
3. `dc logs --since 15m server`

**Likely causes.** An inactive or stalled consumer, a server that is down, a
long-running transaction, or `max_slot_wal_keep_size` approaching its 4 GiB cap.

**Fix.** Restore the consumer (usually the server) and let it catch up. Drop an
orphaned `invalidator_<machine_id>` slot only when its machine has no heartbeat
and the slot is inactive; see [Database](#database). If `wal_status` is
`unreserved` or `lost`, see `ReplicationSlotUnhealthy`.

**Escalate when.** Retained WAL approaches the 4 GiB cap or the disk headroom
alert fires; PostgreSQL may invalidate the slot to protect the disk.

### InvalidatorSlotsMissing

**What it means.** Fewer invalidator slots are active than the expected replica
count for two minutes.

**First three commands.**
1. `sql -c "SELECT slot_name, active, pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS retained FROM pg_replication_slots WHERE slot_name LIKE 'invalidator\\_%' ESCAPE '\\' ORDER BY slot_name;"`
2. `prom 'backendrelay_invalidator_slots_active'`
3. `dc ps server`

**Likely causes.** A server replica is down or still starting, the slot could
not be created because `max_replication_slots` is exhausted, or
`backendrelay_slot_expected_replicas` does not match the real replica count.

**Fix.** Start the missing server replica and confirm it owns an active
`invalidator_<machine_id>` slot. The default expected count is one; for a
cluster, render the exporter file with
`python3 monitoring/slot_queries.py --expected-replicas <n> --output <file>` and
point `POSTGRES_MONITOR_QUERIES_FILE` at it.

**Escalate when.** The server is healthy but no slot is created, or slots cannot
be created because the catalog is full.

### AggregatorSlotMissing

**What it means.** The shared `aggregator` logical replication slot is absent.

**First three commands.**
1. `sql -c "SELECT slot_name, slot_type, active, wal_status FROM pg_replication_slots WHERE slot_name = 'aggregator';"`
2. `dc logs --since 15m server`
3. `dc ps server`

**Likely causes.** First-start race, the slot was dropped manually, the
aggregator lost its advisory lock, or slot creation failed for permissions.

**Fix.** Restart the server; startup recreates a missing aggregator slot and
rebuilds its sketches. Confirm the slot is logical, in this database, and
active. If creation fails, check `max_replication_slots`, the application
role's `REPLICATION` privilege, and `wal2json`.

**Escalate when.** The slot cannot be recreated or aggregator statistics stay
missing.

### ReplicationSlotInactive

**What it means.** A replication slot has had no consumer for ten minutes.

**First three commands.**
1. `sql -c "SELECT slot_name, active, active_pid, restart_lsn FROM pg_replication_slots WHERE NOT active ORDER BY slot_name;"`
2. `prom 'backendrelay_slot_active'`
3. `dc ps server`

**Likely causes.** A stopped server process, an `invalidator_<machine_id>` slot
left by a crash, or the aggregator consumer disconnected.

**Fix.** Restart the owning server. Startup drops inactive `invalidator_%` slots
whose machine has no heartbeat for more than 60 seconds, and the hourly cleanup
removes older ones. Drop a slot manually only when you have confirmed its
machine is gone:

```sh
sql -c "SELECT pg_drop_replication_slot('<slot-name>');"
```

**Escalate when.** An active replica's slot is inactive, or slots accumulate
beyond `max_replication_slots`.

### ReplicationSlotUnhealthy

**What it means.** A replication slot's `wal_status` is not `reserved` for two
minutes. It may be `extended`, `unreserved` or `lost`.

**First three commands.**
1. `sql -c "SELECT slot_name, active, wal_status, invalidation_reason, pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS retained FROM pg_replication_slots WHERE slot_name = 'aggregator' OR slot_name LIKE 'invalidator\\_%' ESCAPE '\\';"`
2. `prom 'backendrelay_slot_wal_status_reserved'`
3. `df -h /var/lib/docker`

**Likely causes.** The consumer is down while WAL grows past
`max_slot_wal_keep_size`, the disk is nearly full, or a long transaction pins
WAL.

**Fix.** Restore the consumer and free disk space. `extended` or `unreserved`
means the slot is at risk; `lost` means the required WAL is gone and the server
must recreate the slot and resynchronise, which it does on restart. Do not
increase `max_slot_wal_keep_size` to hide a stalled consumer.

**Escalate when.** `wal_status` is `lost` or the slot cannot be recreated; users
may have missed invalidations until resync completes.

### RequiredSlotInvalid

**What it means.** A required slot is invalid, wrong type or database, or has no
usable logical position for two minutes.

**First three commands.**
1. `sql -c "SELECT slot_name, slot_type, database, active, invalidation_reason, confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name = 'aggregator' OR slot_name LIKE 'invalidator\\_%' ESCAPE '\\';"`
2. `dc logs --since 15m server`
3. `dc ps server`

**Likely causes.** The slot was recreated with the wrong plugin or database, it
was invalidated by WAL loss, or the consumer never received a confirmed flush
position.

**Fix.** Restart the server so `ensure_logical_replication_slot` recreates the
slot and forces a full resync; for the aggregator, also rebuild its sketches.
Confirm `wal2json` is installed and the slot is logical in the application
database. Never point the server at a physical slot.

**Escalate when.** Recreation fails repeatedly or realtime data is visibly
stale after the resync.

### RequiredSlotStalled

**What it means.** An active consumer has more than 1 MiB pending and has not
advanced `confirmed_flush_lsn` for ten minutes.

**First three commands.**
1. `sql -c "SELECT slot_name, active, pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn)) AS pending, confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name = 'aggregator' OR slot_name LIKE 'invalidator\\_%' ESCAPE '\\';"`
2. `dc logs --since 15m server`
3. `sql -c "SELECT pid, state, wait_event_type, wait_event, left(query, 120) FROM pg_stat_activity WHERE wait_event_type IS NOT NULL;"`

**Likely causes.** The WAL pipeline is blocked on the durable sink or a
database lock, one refresh exceeds its deadline, or the process is stuck without
exiting.

**Fix.** Identify whether the server is writing history or refreshing sessions;
fix the blocking database work first. If the consumer is connected but frozen,
restart it after capturing logs; replay resumes from `confirmed_flush_lsn`.
Check `RealtimeReceiveWait` and `WalRetentionHigh` together.

**Escalate when.** Pending WAL keeps growing toward the slot cap or users stop
receiving updates.

### InvalidationStreamFlapping

**What it means.** The invalidation replication stream restarted more than three
times in ten minutes; readiness stays withheld until the burst leaves the
window.

**First three commands.**
1. `prom 'increase(backendrelay_invalidation_stream_restarts_total[10m])'`
2. `dc logs --since 15m server`
3. `sql -c "SELECT slot_name, active, wal_status, invalidation_reason FROM pg_replication_slots WHERE slot_name = 'aggregator' OR slot_name LIKE 'invalidator\\_%' ESCAPE '\\';"`

**Likely causes.** PostgreSQL restarts, a terminated walsender, a slot
invalidation, network resets, or database load that exceeds the reconnect
backoff.

**Fix.** Realtime keeps working while readiness is withheld; the state clears by
itself once the oldest restart leaves the ten-minute window. Fix the PostgreSQL
cause rather than restarting the server repeatedly. See [WAL health and
invalidation stream flapping](#wal-health-and-invalidation-stream-flapping).

**Escalate when.** Restarts continue beyond 30 minutes, the slot is lost, or
retained WAL rises with it.

### DiskHeadroom

**What it means.** A host filesystem has less than 10 GiB free or less than 15
percent free for five minutes.

**First three commands.**
1. `df -h`
2. `docker system df`
3. `du -sh /var/lib/docker/volumes/*`

**Likely causes.** Retained WAL, container images, Docker logs, old backups, or
database growth. The PostgreSQL data volume and pgBackRest spool are the usual
large consumers.

**Fix.** Delete unused images and build cache, expire old pgBackRest backups
with `dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay expire`,
and check `WalRetentionHigh`. Do not delete files under the PostgreSQL data
directory. If the filesystem cannot be freed, resize the CAX41 disk (see
[Database](#database)).

**Escalate when.** Free space falls below 5 GiB, PostgreSQL stops, or WAL
archiving fails. A full disk can invalidate replication slots.

### BackupStale

**What it means.** The verified off-host backup is missing or older than 24
hours.

**First three commands.**
1. `systemctl list-timers 'backendrelay-*'`
2. `dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay info`
3. `prom 'backendrelay_backup_snapshot_seconds'`

**Likely causes.** The systemd timer is not enabled, the backup failed on
credentials or disk, the healthcheck ping was not written, or the node-exporter
textfile directory is not mounted.

**Fix.** Run a full backup now
(`dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay --type=full backup`),
fix the reported repository or disk error, and enable the timers
(`sudo systemctl enable --now backendrelay-pgbackrest-full.timer
backendrelay-pgbackrest-diff.timer`). Confirm `recovery.py backup` also writes
`backendrelay_backup_snapshot_seconds`.

**Escalate when.** There is no verified off-host backup for 48 hours, or both
repositories are failing.

### WalArchiveFailing

**What it means.** PostgreSQL WAL archiving failed in the last 15 minutes, or
the last archive is over ten minutes old.

**First three commands.**
1. `sql -c "SELECT * FROM pg_stat_archiver;"`
2. `dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay check`
3. `dc logs --since 30m postgres`

**Likely causes.** The object-store repository is unreachable or its
credentials expired, the spool or disk is full, or `archive_command` is wrong.

**Fix.** Fix `pgbackrest.env` or the repository, run `pgbackrest check`, then
force a segment with `sql -c "SELECT pg_switch_wal();"` and confirm
`archived_count` rises and `failed_count` stops. `archive_timeout` is 60
seconds, so the RPO grows while this fails.

**Escalate when.** Archiving stays broken for more than 30 minutes or the last
successful archive is older than the retention needed for point-in-time
recovery.

### AlertDeliveryFailed

**What it means.** Alertmanager could not deliver notifications in the last five
minutes.

**First three commands.**
1. `dc logs --since 15m alertmanager`
2. `dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 alert query`
3. `dc exec -T alertmanager amtool check-config /etc/alertmanager/alertmanager.yml`

**Likely causes.** Pushover user key or application token files are missing or
rotated, the account is rate-limited, or the host cannot reach the Pushover
API.

**Fix.** Restore the secret files and restart Alertmanager. Rotate the Pushover
credentials through the owner's password manager, never inline them. Confirm a
test alert reaches the phone before closing the incident.

**Escalate when.** It happens immediately: paging is down. Use the external
dead-man check and a second channel until delivery works.

### TaskStale

**What it means.** A scheduled task has not succeeded within its expected
interval. Tasks that have never succeeded since process start are excluded here
and caught by TaskFailing.

**First three commands.**
1. `prom 'backendrelay_task_last_success_timestamp_seconds'`
2. `prom 'sum by (task) (increase(backendrelay_task_runs_total{outcome=~"failure|timeout|panic"}[1h]))'`
3. `dc logs --since 1h server`

**Likely causes.** The task is erroring or timing out, the database is slow, a
credential expired, or a worker was disabled by configuration. Cadence groups
range from one hour to three days; read the task name before assuming failure.

**Fix.** Use the logged error for the named task. Common fixes are the database
or object-store credentials, the email provider, or a lock held by a previous
run. Restart the server only if the scheduler itself is wedged.

**Escalate when.** The stale task is `daily-backup`, `hard-deletion-sweep`,
`history-truncator`, `custodian-reaper` or `replication-slot-cleanup`; those
protect data or keep the database bounded.

### TaskFailing

**What it means.** A scheduled task failed in the last hour and has not
succeeded in that hour.

**First three commands.**
1. `prom 'sum by (task) (increase(backendrelay_task_runs_total{outcome=~"failure|timeout|panic"}[1h]))'`
2. `prom 'backendrelay_task_last_success_timestamp_seconds'`
3. `dc logs --since 1h server`

**Likely causes.** A persistent bug or bad input, missing credentials, a
database error, or a task that cannot acquire its lock.

**Fix.** Fix the root cause named in the log, then wait for the next scheduled
run or restart the server to reschedule. Do not repeatedly restart while the
task fails; the log will show the same error.

**Escalate when.** The task is backup, retention, custodian or webhook
retention, or failures have lasted more than a day.

### WebhookDeliveryFailing

**What it means.** Webhook deliveries failed in the last 15 minutes with no
successful delivery in the same window.

**First three commands.**
1. `prom 'sum(increase(backendrelay_webhook_deliveries_total{outcome="failed"}[15m]))'`
2. `dc logs --since 15m server`
3. `prom 'topk(10, backendrelay_app_webhook_events_failed)'`

**Likely causes.** The app's endpoint is down, DNS or TLS fails, the SSRF guard
blocks a private destination, or the receiver rejects the signature.

**Fix.** Identify the app from the per-app metric, ask its owner to check the
endpoint, and allow a trusted private destination with the
`smokescreen-whitelist-ips` flag only when it is intentional. Retries continue;
inspect the delivery error in the server log.

**Escalate when.** All apps fail, which points at the network, signing key, or
platform rather than one receiver.

### EmailFailing

**What it means.** Email sends failed in the last 15 minutes with no successful
send in the same window.

**First three commands.**
1. `prom 'sum(increase(backendrelay_emails_total{outcome="failed"}[15m]))'`
2. `dc logs --since 15m server`
3. `dc exec -T server /usr/local/bin/instant-server healthcheck`

**Likely causes.** The provider is down, its API token expired, the sender
domain is unverified, or the provider rejected the message. When no provider is
configured, codes are logged at WARN instead of sent.

**Fix.** Check the provider status, rotate the API token, and verify the sender
domain. With Cloudflare Email Sending, confirm the zone's Email Sending
subdomain is verified and that `INSTANT_APP_EMAIL_SENDER_EMAIL` is set. The
dashboard shows the per-domain error.

**Escalate when.** Login or magic-code email is undeliverable, or the failure
affects every app.

### StorageErrors

**What it means.** More than five percent of object-store or database storage
operations failed in the last ten minutes. Client rejections are excluded.

**First three commands.**
1. `prom 'sum by (op) (increase(backendrelay_storage_ops_total{outcome="error"}[10m]))'`
2. `dc logs --since 15m server`
3. `dc exec -T server /usr/local/bin/instant-server healthcheck`

**Likely causes.** R2 is unavailable or rate-limiting, credentials or bucket
names changed, the storage quota rejects uploads (a 403, not an error), or the
database behind file metadata is failing.

**Fix.** Check the provider status and the server's S3 credentials. Confirm the
files bucket exists and the CORS and public endpoint are unchanged. Retry the
failed operation; storage operations are idempotent at the object key.

**Escalate when.** Uploads or downloads fail broadly, or file metadata and
objects diverge.

### CanaryFailing

**What it means.** The canary has not completed a write-to-refresh probe in
five minutes.

**First three commands.**
1. `dc ps canary`
2. `dc logs --since 15m canary`
3. `prom 'backendrelay_canary_last_success_timestamp_seconds'`

**Likely causes.** The canary profile is not running, the API URL or app
credentials are wrong, the canary app was deleted, or the public edge/TLS path
is broken.

**Fix.** Start the profile
(`dc --profile canary up -d canary`), check `CANARY_API_URL`, `CANARY_APP_ID`
and the token file, and verify `/health/system` from outside. The canary talks
to the public origin, so a healthy internal server can still fail this probe.

**Escalate when.** The public API is also failing or the canary cannot
authenticate; this is an outside-in outage signal.

### CanaryPropagationSlow

**What it means.** Realtime canary propagation p95 exceeds five seconds for ten
minutes.

**First three commands.**
1. `prom 'histogram_quantile(0.95, sum(rate(backendrelay_canary_propagation_seconds_bucket[10m])))'`
2. `prom 'backendrelay_wal_retained_bytes'`
3. `dc logs --since 15m canary`

**Likely causes.** WAL apply or the durable sink is slow, the database is
overloaded, the edge adds latency, or one app's refresh backlog delays the
window.

**Fix.** Check `RealtimeReceiveWait`, `PoolPressure` and the per-app database
time. The canary is a single write-to-refresh path; a slow database or WAL
consumer shows up here first. Fix the shared cause rather than the canary
interval.

**Escalate when.** p95 exceeds ten seconds, the canary starts failing, or the
delay coincides with user-visible stale data.

## Database

D4 runs PostgreSQL 17 self-managed on the CAX41 in the `postgres` service.
There is no provider console, managed failover or provider resize; use the
commands below and the compose files in `self-hosting/`.

### Inspect replication slots

The server owns one logical slot per process, `invalidator_<machine_id>`, where
`machine_id` is generated for each process start. The shared aggregator slot is
named `aggregator`. Both use `wal2json` in the application database.

```sh
sql -c "SELECT slot_name, plugin, slot_type, database, active, active_pid,
               restart_lsn, confirmed_flush_lsn, wal_status,
               invalidation_reason,
               pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS retained
          FROM pg_replication_slots
         WHERE slot_name = 'aggregator'
            OR slot_name LIKE 'invalidator\\_%' ESCAPE '\\'
         ORDER BY slot_name;"
```

`active` means a consumer is connected. `retained` is WAL the slot still needs;
`backendrelay_wal_retained_bytes` is the maximum across slots and alerts above
1 GiB. `wal_status` other than `reserved` is covered by
`ReplicationSlotUnhealthy`. Inactive invalidator slots from machines without a
heartbeat are dropped at startup; the hourly cleanup covers older leftovers.

For a cluster, the exporter expects one replica by default. Render the queries
with the real count and point `POSTGRES_MONITOR_QUERIES_FILE` at the output:

```sh
cd self-hosting
python3 monitoring/slot_queries.py --expected-replicas 2 --output slot-queries.yml
```

### Backups and restore

Inspect and verify the pgBackRest repository before maintenance:

```sh
dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay info
dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay check
```

A physical restore must go into an empty, disposable target, never over the
live data directory. The full procedure, including point-in-time restore with
`--target-action=promote`, is in
[`self-hosting/backup/README.md`](../self-hosting/backup/README.md). The short
form after creating an empty volume and restore container is:

```sh
docker exec -u postgres backendrelay-restore pgbackrest --stanza=backendrelay restore
docker exec -u postgres backendrelay-restore pg_ctl -D /var/lib/postgresql/data start
```

The encrypted logical bundle from `recovery.py` remains the independent copy
when pgBackRest is unavailable or original app identities are needed. Restore
it with `recovery.py restore --bundle <file> --identity-file <age-key>
--target-config <file>` as described in
[`docs/self-hosting.md`](self-hosting.md#backups-and-recovery).

### Resize the CAX41

The database is a Docker named volume (`backend-db`) on the host root disk, so
there is no database-level volume to resize.

1. Take a backup and confirm it verifies:
   `dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay --type=full backup`.
2. In the Hetzner Cloud Console, resize the CAX41 to the larger plan (owner
   action; it may require a shutdown). Do not recreate the server or its disk.
3. After it boots, confirm the kernel and filesystem see the larger disk:

   ```sh
   lsblk
   df -h /
   ```

4. Grow the partition and filesystem. On the CAX41 Ubuntu image the root disk
   is normally `/dev/sda`:

   ```sh
   sudo growpart /dev/sda 1
   sudo resize2fs /dev/sda1
   df -h /
   ```

5. Start PostgreSQL and the server, then check the database size and
   `DiskHeadroom`:

   ```sh
   dc up -d
   dc exec -T server /usr/local/bin/instant-server healthcheck
   ```

If the filesystem is XFS, use `sudo xfs_growfs /` instead of `resize2fs`. If the
device names differ, use the values from `lsblk`; do not create a new
filesystem.

### Restart with a drain

Restart PostgreSQL only after the server has stopped accepting new sessions and
finished draining. The server sends 1012 to WebSockets and keeps the process
alive for up to `INSTANT_DRAIN_BUDGET_SECONDS` (30 s by default), plus the
compose `stop_grace_period: 45s`.

```sh
dc stop --timeout 45 server
dc restart postgres
dc up -d server
dc exec -T server /usr/local/bin/instant-server healthcheck
```

If `dc restart postgres` returns before PostgreSQL is ready, wait for
`dc exec -T postgres pg_isready --username postgres --dbname instant`. The
server's pools and replication streams reconnect with backoff. There is no HA
switchover in this topology; `failover` and `NEXT_DATABASE_URL` apply only to a
configured replica pair.

For a host reboot, stop the server first to drain, then reboot. The base compose
file does not define a restart policy for `server`, `www` or `postgres`, so run
`dc up -d` after the host is back and verify health.

## Secrets

No secret value belongs in git. The server keys live in the `server_secrets`
volume, deployment credentials live in the mode-0600 `self-hosting/deploy/.env`
(and the staging project's `deploy/.env.staging`), and monitoring, canary and
backup credentials live in root-owned mode-0600 files under `/etc/backendrelay`.
Every value also goes into the password manager, and the server keys get one
age-encrypted copy off host.

In short:

- `INSTANT_AEAD_KEYSET_JSON` encrypts app OAuth client secrets and BYOP
  connection strings.
- `INSTANT_WEBHOOK_KEYSET_JSON` signs webhook payloads and is published as
  JWKS.
- `INSTANT_RATE_LIMIT_HMAC_KEY` derives durable rate-limit bucket IDs.

### Where each secret lives

Names only; open the password manager for values.

| Secret | Lives in | Independent copy |
| --- | --- | --- |
| `INSTANT_AEAD_KEYSET_JSON` | `server_secrets` volume, `/home/instant/config/secrets.env` (0600) | password manager; age-encrypted backup in R2 and offline |
| `INSTANT_WEBHOOK_KEYSET_JSON` | same file | same |
| `INSTANT_RATE_LIMIT_HMAC_KEY` | same file | same |
| staging `INSTANT_AEAD_KEYSET_JSON`, `INSTANT_WEBHOOK_KEYSET_JSON`, `INSTANT_RATE_LIMIT_HMAC_KEY` | `backendrelay-staging_server_secrets` volume, same file name (0600) | password manager; independent of production |
| `POSTGRES_PASSWORD`, `INSTANT_DB_PASSWORD` | `deploy/.env` (0600) | password manager |
| `AUX_POSTGRES_PASSWORD`, `AUX_DB_PASSWORD` | staging `deploy/.env.staging` (0600) | password manager |
| `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | `deploy/.env` (0600); the staging bucket tokens in `deploy/.env.staging` | password manager; R2 token inventory |
| `CLOUDFLARE_EMAIL_API_TOKEN` | `deploy/.env` (0600) | password manager |
| `CLOUDFLARE_DNS_API_TOKEN` | `deploy/.env` (0600, only with the DNS-01 Caddyfile) | password manager |
| `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET` | `deploy/.env` (0600) | password manager |
| `INSTANT_DASHBOARD_GATEWAY_TOKEN` | `deploy/.env` (0600, only in private gateway mode) | password manager |
| `POSTMARK_TOKEN`, `POSTMARK_ACCOUNT_TOKEN`, `SENDGRID_TOKEN`, `RESEND_TOKEN` | `deploy/.env` (0600, legacy providers only) | password manager |
| `PUSHOVER_USER_KEY_FILE`, `PUSHOVER_TOKEN_FILE`, `DEADMAN_WEBHOOK_FILE`, `GRAFANA_PASSWORD_FILE`, `POSTGRES_MONITOR_PASSWORD_FILE`, `BUGSINK_SECRET_KEY_FILE` | root-owned 0600 files named by those variables, by default under `/etc/backendrelay` | password manager |
| canary admin token (path in `CANARY_ADMIN_TOKEN_FILE`) | root-owned 0600 file; recreate the token if lost | password manager |
| `PGBACKREST_REPO1_CIPHER_PASS`, `PGBACKREST_REPO1_S3_KEY`, `PGBACKREST_REPO1_S3_KEY_SECRET` (and repo 2) | `self-hosting/pgbackrest.env` (0600) | password manager |
| R2 secrets-backup token (rclone/S3 config) | root-owned 0600 file used only by `secrets-backup.sh`; scoped to `instantdb-secrets` | password manager |
| GHCR pull token | `/root/.docker/config.json` (0600) | password manager |
| age identity (not a BackendRelay variable) | offline medium and password manager only; never on the host | not applicable |

The `server_secrets` file is the only copy of the keysets that can decrypt
stored OAuth client secrets and BYOP connection strings. Losing it makes that
data unreadable even when the database backup is intact.

### Generate `secrets.env` (first install only)

Do this once per deployment, with the signed server image, before the first
`serve`. Never let two instances generate their own file: the entrypoint creates
one if the volume is empty, and a second one would hold different keys.

1. Create an age key pair on your workstation, outside the repository. Keep the
   identity offline; only the public recipient goes on the host.

   ```sh
   age-keygen -o "$HOME/backendrelay-age.key"
   ```

   `age-keygen` prints the `age1...` recipient. Move the identity file to the
   offline medium, store a copy in the password manager, and remove the local
   copy.

2. Generate the three keys. The command reads no server configuration and
   prints a shell-sourceable file:

   ```sh
   cd /opt/backendrelay/self-hosting
   umask 077
   docker compose --project-name backendrelay --project-directory . \
     --env-file deploy/.env -f deploy/docker-compose.yml \
     run --rm --no-deps -T server generate-secrets \
     > /tmp/secrets.env
   ```

   `backendrelay` is the default `DEPLOY_COMPOSE_PROJECT`; use your project
   name if you changed it, because the volume name depends on it.

3. Check the shape without printing values (this prints `3`):

   ```sh
   grep -c '^INSTANT_\(AEAD_KEYSET_JSON\|WEBHOOK_KEYSET_JSON\|RATE_LIMIT_HMAC_KEY\)=' \
     /tmp/secrets.env
   ```

4. Copy the file into the compose volume. The one-shot container uses the
   server image's own non-root user and the volume's existing 0700 directory:

   ```sh
   docker compose --project-name backendrelay --project-directory . \
     --env-file deploy/.env -f deploy/docker-compose.yml \
     run --rm --no-deps -T --entrypoint /bin/sh server \
     -c 'umask 077 && cat > /home/instant/config/secrets.env' < /tmp/secrets.env
   docker compose --project-name backendrelay --project-directory . \
     --env-file deploy/.env -f deploy/docker-compose.yml \
     run --rm --no-deps -T --entrypoint /bin/sh server \
     -c 'stat -c "%a" /home/instant/config/secrets.env'
   ```

   The last command prints `600`. Confirm the file has three assignments, then
   remove the temporary copy with `shred -u /tmp/secrets.env`.

5. Store the file content in the password manager as a secure note or
   attachment, then take the first [secrets backup](#back-up-server_secrets).

Never run `generate-secrets` into a volume that already has `secrets.env` and
stored ciphertext: a newly generated keyset cannot decrypt the old rows. Use
[Key rotation](#key-rotation) instead.

### The deployment `.env`

`self-hosting/deploy/.env` holds the compose and deploy credentials listed
above. Copy `.env.example`, fill it in, and lock it down:

```sh
chmod 600 /opt/backendrelay/self-hosting/deploy/.env
stat -c '%a' /opt/backendrelay/self-hosting/deploy/.env   # 600
```

It is in `.gitignore`; `deploy.sh` refuses a file that group or other can read or
write. The server keysets do not belong in this file: the container reads them
from `$INSTANT_CONFIG_DIR/secrets.env` in the volume.

The staging project uses the same rules with `deploy/.env.staging`, copied from
`.env.staging.example` (also git-ignored). Keep it separate from `.env` and
select it with `DEPLOY_ENV_FILE`; see
[`deploy/README.md#staging-on-the-same-host-p75`](../self-hosting/deploy/README.md#staging-on-the-same-host-p75).

### Back up `server_secrets`

The `recovery.py` bundle already contains `server-secrets.tar`, and it is the
right copy for a full database restore. Keep an independent one as well: the
bundle shares a bucket and credentials with the database copy, so one object
store accident can take both. Encrypt the keys with age and keep one copy in R2
and one offline.

Run the backup from the repository checkout as root, with the offline identity
mounted only long enough to verify the round trip:

```sh
sudo SECRETS_AGE_RECIPIENT='age1...' \
  SECRETS_AGE_IDENTITY_FILE=/media/offline/backendrelay-age.key \
  SECRETS_UPLOAD_COMMAND='rclone copy "$BUNDLE" r2:instantdb-secrets/ && rclone copy "$CHECKSUM" r2:instantdb-secrets/' \
  SECRETS_VERIFY_COMMAND='rclone lsf r2:instantdb-secrets/ | grep -qx "$BUNDLE_NAME"' \
  bash self-hosting/backup/secrets-backup.sh
```

Use the dedicated `instantdb-secrets` bucket with its own scoped token. Do not use
`instantdb-backups` (its lifecycle rule expires every object after 30 days) or
the pgBackRest repository (it shares credentials with the database backups).

[`secrets-backup.sh`](../self-hosting/backup/secrets-backup.sh) copies the
`server_secrets` volume (or `SECRETS_VOLUME`), writes
`/var/lib/backendrelay/secrets-backups/secrets-<timestamp>-<host>.tar.age` and a
`.sha256` sidecar, and never prints a value. It reads the volume directly, so it
also works before the first `serve`. `rclone` is an example; any command that
copies a local file works, including `aws s3 cp` with the R2 endpoint. If
`SECRETS_AGE_IDENTITY_FILE` is unset, the script warns that verification is
deferred.

Copy the same two files to the offline medium:

```sh
sudo cp /var/lib/backendrelay/secrets-backups/secrets-<timestamp>-<host>.tar.age \
  /var/lib/backendrelay/secrets-backups/secrets-<timestamp>-<host>.tar.age.sha256 \
  /media/offline/
```

Take a new copy after the first generation, after every rotation, before a host
rebuild, and monthly. Do not delete the previous copy until the new one is
verified in both places. `recovery.py` retention does not cover this artifact;
prune old local files yourself once the off-host copies are confirmed.

### Restore `server_secrets`

Use this when the volume is lost or when rebuilding the host; for a full
database restore, `recovery.py restore` also writes the configuration
directory.

1. Download the bundle and its sidecar, then check it:

   ```sh
   sha256sum -c secrets-<timestamp>-<host>.tar.age.sha256
   ```

2. Decrypt with the offline identity and extract the file without printing it:

   ```sh
   age --decrypt -i /media/offline/backendrelay-age.key \
     -o /tmp/secrets.tar secrets-<timestamp>-<host>.tar.age
   tar -xOf /tmp/secrets.tar config/secrets.env > /tmp/secrets.env
   chmod 600 /tmp/secrets.env
   ```

3. Stop the server, install the file into the volume with the same one-shot
   container as above, then start the server:

   ```sh
   docker compose --project-name backendrelay --project-directory . \
     --env-file deploy/.env -f deploy/docker-compose.yml stop server
   docker compose --project-name backendrelay --project-directory . \
     --env-file deploy/.env -f deploy/docker-compose.yml \
     run --rm --no-deps -T --entrypoint /bin/sh server \
     -c 'umask 077 && cat > /home/instant/config/secrets.env' < /tmp/secrets.env
   docker compose --project-name backendrelay --project-directory . \
     --env-file deploy/.env -f deploy/docker-compose.yml start server
   shred -u /tmp/secrets.env /tmp/secrets.tar
   ```

4. Replace the volume copy with the restored file even when a new keyset could
   be generated: new keys cannot read old ciphertext. Verify with
   `instant-server reencrypt --dry-run` (it must run, not report an AEAD error)
   and a dashboard sign-in through an app with a stored OAuth secret.

### Verify the off-host copy

Monthly, fetch the newest `.tar.age` and `.sha256` from R2 to a scratch
directory, run `sha256sum -c`, decrypt with the offline identity, and compare
the result with the live file:

```sh
cmp -s /tmp/secrets.env /home/instant/config/secrets.env
```

A difference is expected only when a rotation happened after the backup; in
that case check that the decrypted file still has the three assignments and
take a fresh backup. Record the date and result with the other drills in
[Failure drills](#failure-drills).

Rotate with `generate-secrets --rotate` and `reencrypt` following [Key
rotation](#key-rotation); never edit or delete the only copy in place.

## Provisioning the production host

Production is one CAX41 running Docker Compose with Cloudflare in front and a
self-managed PostgreSQL 17 (D4 = A). The ordered runbook is
[`self-hosting/provision/README.md`](../self-hosting/provision/README.md):
rebuild from a clean Ubuntu 24.04 image, attach the Hetzner Cloud Firewall,
bootstrap the host, configure Cloudflare DNS/TLS/WAF, R2, the database, email
and GHCR pull access.

- **Rebuilding destroys the disk.** The old Coolify deployment, its database and
  its secrets go with it. Do not reuse them. Confirm the server type and the
  legacy price in the console before you rebuild.
- **Two firewalls.** The Hetzner Cloud Firewall is authoritative. Docker
  publishes ports through its own iptables chain, so `ufw` alone does not filter
  container traffic. Update both lists when your admin address or Cloudflare's
  ranges change.
- **Host hardening.** `sudo ADMIN_SSH_CIDR=<admin-ip> bash
  self-hosting/provision/host-bootstrap.sh` installs the packages, makes SSH
  key-only, applies the firewall, installs host-wide Docker log limits that
  match the P2.11 per-service settings, enables unattended security updates and
  creates the deploy directories. Re-run it with `--check` to verify without
  changing anything.
- **Certificates.** Cloudflare SSL mode is Full (strict), so the origin
  certificate in `DEPLOY_TLS_DIR` must cover `api.` and `dash.`.
- **Email.** Cloudflare Email Sending is the provider (D6). The server refuses
  to start without `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_EMAIL_API_TOKEN` and
  `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL`.
- **Registry.** The images are private in GHCR. Log in as the deploy user with a
  read-only package token before the first pull.

Unattended upgrades keep automatic reboot off because a reboot interrupts live
sessions. Apply a pending reboot with the drain procedure above.

## Deploys

Images are referenced by digest, not by a mutable tag:

```sh
INSTANT_SERVER_IMAGE=registry.example.com/backendrelay/server@sha256:<digest>
INSTANT_DASHBOARD_IMAGE=registry.example.com/backendrelay/dashboard@sha256:<digest>
```

Keep those two lines in `self-hosting/.env` (mode 0600) and run compose from
`self-hosting/`. The binary verifies the schema at startup and never applies
migrations; `migrate up` is a separate step.

The production deployment kit in [`self-hosting/deploy/`](../self-hosting/deploy/README.md)
automates the steps below: it verifies signatures, takes a backup that must
succeed, migrates, restarts one replica at a time, smoke-tests the public edge,
and rolls back to the previous digests on failure. Use the procedure below as
the manual fallback or when changing the kit itself.

### Rehearse on staging

A second compose project, `backendrelay-staging`, runs on the same host with the
same compose file and the same `deploy.sh`. It has its own env file
(`self-hosting/deploy/.env.staging`), network and subnet, host ports, database in
`aux-postgres`, R2 buckets, `server_secrets` volume and rollback state. Its
public hostnames are `staging-api.<domain>` and `staging-dash.<domain>`.

```sh
DEPLOY_ENV_FILE=deploy/.env.staging bash deploy/deploy.sh <server> <dashboard>
```

Rehearse every release there first, then deploy the same digests to production.
`DEPLOY_STACK=staging` makes the scripts use `aux-postgres`, enable the
`staging` profile and refuse a file that reuses the production project name,
network name or state directory. Staging keeps five local `pg_dumpall` dumps
and takes no off-host backup; its data is disposable. Setup, ports (8080/8443
behind a Cloudflare Origin Rule) and the rehearsal procedure are in
[`deploy/README.md#staging-on-the-same-host-p75`](../self-hosting/deploy/README.md#staging-on-the-same-host-p75).

### Deploy

1. Set `INSTANT_SERVER_IMAGE` and `INSTANT_DASHBOARD_IMAGE` in
   `self-hosting/.env` to the new digests.
2. Back up the database and the `server_secrets` volume, and confirm the
   backup. Do not skip this when a migration is in the release.
3. Pull the new images and apply their migrations with the **new** server
   image:

   ```sh
   docker compose -f docker-compose.with-caddy.yml pull server www
   docker compose -f docker-compose.with-caddy.yml run --rm server migrate up
   ```

4. Recreate the server, then the dashboard:

   ```sh
   docker compose -f docker-compose.with-caddy.yml up -d --no-deps server
   docker compose -f docker-compose.with-caddy.yml exec -T server /usr/local/bin/instant-server healthcheck
   docker compose -f docker-compose.with-caddy.yml up -d --no-deps www
   ```

5. Check `https://api.<domain>/health/system` returns `{"wal":"ok"}`, open the
   dashboard, and run one canary or SDK query. `instant-server audit` reports
   app-rules hygiene and exits non-zero on findings:

   ```sh
   docker compose -f docker-compose.with-caddy.yml exec -T server /usr/local/bin/instant-server audit
   ```

The server drains live sessions for up to 30 seconds on SIGTERM and the compose
file allows 45 seconds before SIGKILL.

### Roll back

An older binary can serve a newer schema only while every migration between
them is marked `-- compat: additive`. Check the recorded flags:

```sh
sql -c "SELECT version, additive FROM instant_migration_checksums WHERE version > <previous-version> ORDER BY version;"
```

- **All additive.** Put the previous digests in `self-hosting/.env` and recreate
  the server and dashboard. No migration step is needed.
- **Any non-additive.** Run `migrate down <steps>` with the **new** image before
  starting the old one, because the old binary refuses a newer schema:

  ```sh
  INSTANT_SERVER_IMAGE=<new-digest> docker compose -f docker-compose.with-caddy.yml run --rm server migrate down <steps>
  ```

  Then set the previous digest and recreate `server` and `www`. `migrate down`
  runs the migration's `.down.sql`; if it would lose data you need, restore the
  backup instead of rolling the schema back.

`run-compose-image-rollback.sh` is the repository's acceptance test for this
flow: it migrates with the candidate, reads existing data with the baseline,
and restores the candidate on the same PostgreSQL volume. The expand → migrate
→ contract policy and the additive marker are in
[development.md](development.md#schema-changes-and-rollbacks).

## First production deploy (P7.6)

Going live means five things: the first signed release serves production, the
dashboard admits only the owner, the initial config flags are pinned, the canary
app exists, and `instant-server audit` is clean. Rehearse the same digests on
staging first ([Rehearse on staging](#rehearse-on-staging)). The kit, its
prerequisites and the bootstrap command are in
[`self-hosting/deploy/README.md`](../self-hosting/deploy/README.md).

### Restrict the dashboard

Set these in `deploy/.env` (mode 0600) before the first deploy:

```sh
INSTANT_DASHBOARD_SIGNUP=closed
INSTANT_DASHBOARD_ALLOWED_EMAILS=<owner-email>
INSTANT_SUPERUSER_EMAIL=<owner-email>
INSTANT_EPHEMERAL_APPS=false
# Optional private gateway mode; leave both blank until the gateway exists.
INSTANT_DASHBOARD_PRIVATE=
INSTANT_DASHBOARD_GATEWAY_TOKEN=
```

- `INSTANT_DASHBOARD_ALLOWED_EMAILS` is the allow list for dashboard sign-in
  and sign-up, Google logins included; the `INSTANT_SUPERUSER_EMAIL` address is
  always accepted, and listed addresses may sign up while signup stays closed.
  Application end-user authentication is unaffected. A blank value removes the
  restriction, so production should keep it filled.
- `INSTANT_EPHEMERAL_APPS=false` is already the shipped default. The config flag
  below pins it so a later environment change cannot re-enable unauthenticated
  app creation.
- **Private gateway mode is optional.** `INSTANT_DASHBOARD_PRIVATE=true` with
  `INSTANT_DASHBOARD_GATEWAY_TOKEN` (32–128 characters from `A–Z a–z 0–9 - _`)
  requires the matching `x-instant-dashboard-gateway-token` header on every
  dashboard, superadmin and platform-OAuth request. The kit's Caddy does not
  inject that header, so enable the mode only after the gateway in front of the
  dashboard does; otherwise the dashboard is unreachable. Generate a token with
  `openssl rand -hex 32`. It is a second lock on the management surface, not a
  replacement for the allow list: browsers behind the gateway still sign in.

### Deploy the first release

```sh
cd /opt/backendrelay/self-hosting
DEPLOY_FIRST_DEPLOY=1 DEPLOY_SKIP_CANARY_SMOKE=1 bash deploy/deploy.sh \
  registry.example.com/backendrelay/server@sha256:<digest> \
  registry.example.com/backendrelay/dashboard@sha256:<digest>
```

Both variables are one-time bootstrap switches. `DEPLOY_FIRST_DEPLOY=1` skips
the pre-deploy backup because the database is empty; every later deploy must
take the verified backup. `DEPLOY_SKIP_CANARY_SMOKE=1` is valid only until the
canary app exists, and the canary step below drops it.

The run verifies both signatures, migrates, recreates the server and dashboard,
checks the public `/health/system`, the client-IP chain and the audit, then
writes `current.env` under `/var/lib/backendrelay/deploy`.

### Pin the initial config flags

The one-shot containers below run the server image recorded by the last
successful deploy. `flags set` writes through the native transaction path, so
cached policy reads are invalidated; the slug map is read when the routes are
assembled and needs a restart.

```sh
cd /opt/backendrelay/self-hosting
kit() {
  docker compose --project-name backendrelay --project-directory . \
    --env-file deploy/.env \
    --env-file /var/lib/backendrelay/deploy/current.env \
    -f deploy/docker-compose.yml "$@"
}

# Keep unauthenticated app creation off regardless of the environment.
kit run --rm --no-deps -T server flags set ephemeral-apps-enabled false

# Give the apps you operate a stable label in the per-app metrics.
kit run --rm --no-deps -T server flags set app-metric-slugs \
  '{"<app-id>":"<app-slug>"}'

kit run --rm --no-deps -T server flags get ephemeral-apps-enabled
kit run --rm --no-deps -T server flags get app-metric-slugs
```

`app-metric-slugs` maps app UUIDs to the `app` label on the per-app Grafana
panels and the `backendrelay_app_*` series: lowercase letters, digits and `-`,
at most 32 bytes, never `other`. An app without a slug keeps the first eight
characters of its UUID, at most 100 resolved apps get their own label, and the
rest report as `other`. Add the canary app and each app you onboard after it
exists.

Restart the server once so the slug map is loaded. On a serving deployment the
same restart drains SSE sessions for up to 30 seconds.

```sh
kit up -d --no-deps --force-recreate server
kit exec -T server /usr/local/bin/instant-server healthcheck
```

The recognized settings are listed in
[configuration.md](configuration.md#config-app-flags).

### Create the canary app

Create the app in the dashboard while signed in as the owner, copy its app id
and admin token, write the token to a root-owned 0600 file and fill
`CANARY_APP_ID` and `CANARY_ADMIN_TOKEN_FILE` in `deploy/.env` exactly as
described in [Canary](#canary). Do not reuse an application's credentials.

Then deploy the same digests again without the skip:

```sh
bash deploy/deploy.sh <server> <dashboard>
```

This time the smoke test runs one SDK query against the canary app, so a broken
canary token or a closed runtime path fails the deploy and rolls it back. Add
the canary app to `app-metric-slugs` and restart the server as above if you want
its own label. The canary container itself starts with P7.7.

### Run the audit

Every deploy already ends with an audit. On the first release, run it once by
hand and read the summary:

```sh
kit exec -T server /usr/local/bin/instant-server audit
```

(`kit` is the helper defined in the flags section above; without it, add the
`--env-file` arguments from
[Image state](../self-hosting/deploy/README.md#image-state) to a
`docker compose ... exec -T server` command.)

The last line is the summary, for example
`{"severity":"info","check":"summary","errors":0,"warnings":1}`. Warnings do not
fail a deploy: a newly created app without a rules document reports
`app-no-rules`, and an app without an owner reports `app-no-owner`. Errors do,
and the command exits 1 on any: the config app is missing, unlocked (admin
tokens, members or invites) or no longer carries migration 146's deny-all rules,
or an app's `$default` allows everything. Fix the finding rather than skipping
the check; the deploy fails on the same audit and
`backendrelay-deploy-audit.timer` reruns it weekly.

### Go-live checklist

- [ ] The same digests passed on staging, including one rehearsed rollback.
- [ ] `deploy/.env` is 0600; `INSTANT_DASHBOARD_ALLOWED_EMAILS` and
      `INSTANT_SUPERUSER_EMAIL` name the owner; signup is closed.
- [ ] Private gateway mode, if used, has a generated token and a gateway that
      injects the header; otherwise it is blank.
- [ ] The first deploy ran with `DEPLOY_FIRST_DEPLOY=1` and
      `DEPLOY_SKIP_CANARY_SMOKE=1`.
- [ ] `ephemeral-apps-enabled` is `false`, `app-metric-slugs` is set, and the
      server was restarted after the slug change.
- [ ] The canary app and its token file exist; a deploy without the skip passes
      the canary SDK query.
- [ ] `instant-server audit` reports zero errors.
- [ ] An address that is not on the allow list is refused at dashboard sign-in,
      and the owner address can sign in.
- [ ] Continue with [P7.7](#monitoring-go-live-and-alert-tests-p77): the
      monitoring overlay, external checks, the canary and the alert tests.

## Monitoring go-live and alert tests (P7.7)

The monitoring overlay, log shipping, external checks and the canary are live
only when a critical alert reaches the phone, a stopped Alertmanager is noticed
from outside, and the WAL archive alert is proven to fire. Work through this
section after [the first production deploy](#first-production-deploy-p76), and
repeat the alert, dead-man and WAL tests whenever Alertmanager, Pushover, the
external checks or the backup repository change.

### Start the overlay and the canary

```sh
cd /opt/backendrelay/self-hosting
DEPLOY_MONITORING=1 bash deploy/deploy.sh <server> <dashboard>
sudo systemctl enable --now backendrelay-deploy-canary.service
```

`deploy/.env` must already contain the digest-pinned monitoring images, the
Pushover user key and application token files, the dead-man URL file, the
Grafana and exporter passwords, and the canary app and token file from
[Create the canary app](#create-the-canary-app); the variable list is in
[Monitoring and health](self-hosting.md#monitoring-and-health). Pushover and the
dead man read their credentials from the mounted files, never from the
environment. The canary unit starts the `canary` profile; without it Prometheus
reports a missing `canary` target and `ServiceUnavailable` fires.

### Check the wiring

`monitoring/live-check.sh` is read-only. It checks the overlay services, every
Prometheus target, the three `pg_stat_archiver_*` series, an idle
`WalArchiveFailing`, a firing `Watchdog`, a fresh canary probe and the backup
timestamps, and exits non-zero on every gap:

```sh
bash monitoring/live-check.sh
```

With the deployment kit, pass its files through:

```sh
bash monitoring/live-check.sh --project-name backendrelay --project-directory . \
  --env-file deploy/.env -f deploy/docker-compose.yml -f docker-compose.monitoring.yml
```

Log shipping is a separate path from metrics. Open Grafana Explore
(`http://127.0.0.1:3001`, datasource **Loki**) and confirm recent lines from the
server and dashboard containers (`{container=~".*"}`); Prometheus being up does
not prove that Alloy ships logs. The pipeline is described under [Logs](#logs).

### Fire a test alert to the phone

Use the Alertmanager CLI, so the test does not depend on a rule or a real fault:

```sh
dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 \
  alert add alertname=OperationalTestAlert severity=critical \
  --annotation=summary='P7.7 phone test'
```

Confirm that the critical Pushover notification arrives and acknowledge it on the
phone: priority 2 repeats until acknowledged. Confirm the resolved notification
when the alert's end time passes. If nothing arrives, `AlertDeliveryFailed` and
its [playbook](#alertdeliveryfailed) cover the next steps.

### Test the dead man

The `Watchdog` rule fires into the external dead-man receiver on every evaluation,
so healthchecks.io notices a silent Alertmanager. Stop Alertmanager for ten
minutes and watch the check go down:

```sh
dc stop alertmanager
# wait ten minutes: the healthchecks.io check must go down and page
dc start alertmanager
dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 alert query
```

Give the check a period and grace shorter than ten minutes; with the shipped
route the Watchdog is repeated every minute. Confirm that the check recovers
after `dc start alertmanager`. If it does not, the dead-man URL file or outbound
HTTPS is broken, not the alert rules.

### Confirm the WAL archive metrics

`WalArchiveFailing` reads `pg_stat_archiver_failed_count`,
`pg_stat_archiver_archived_count` and `pg_stat_archiver_last_archive_age` from
the postgres-exporter's built-in archiver collector. `live-check.sh` fails when
any of the three is missing, which is the failure mode to catch here: a pinned
exporter without the collector leaves the rule silently dead.

Then prove that a rising failure count reaches the alert and the phone without
touching PostgreSQL. The helper writes a monotonic counter through
node-exporter's textfile collector, which is the same series name the rule
matches:

```sh
sudo bash monitoring/fire-wal-alert.sh
```

It refuses to start if `WalArchiveFailing` is already firing, waits up to ten
minutes for the rule (`for: 5m`), confirms that Alertmanager has the alert, holds
it for five minutes, and then removes the file so that it resolves. Confirm the
critical notification, acknowledge it, and check that
`prom 'ALERTS{alertname="WalArchiveFailing"}'` returns to empty after the next
scrape.

**Real archiving failure.** Repeat the check against a database whose archive
target is disposable. Production keeps archiving: breaking it there retains WAL
and risks the live backup. Staging does not archive WAL today (`aux-postgres` has
no pgBackRest repository and `deploy/.env.staging` sets
`INSTANT_PG_ARCHIVE_MODE=off`), so this needs staging archiving and an exporter
scraped by a Prometheus first. Once that exists, point the staging repository key
at a wrong value, restart the staging database, and read the counters:

```sh
docker compose --project-name backendrelay-staging --project-directory . \
  --env-file deploy/.env.staging --profile staging \
  -f deploy/docker-compose.yml exec -T aux-postgres \
  psql -U postgres -d staging -Atc \
  'SELECT archived_count, failed_count, last_archived_time FROM pg_stat_archiver;'
```

`failed_count` rises, `last_archive_age` stops advancing, and the alert fires
after five minutes. Revert the key and confirm that the next archive succeeds
before moving on.

### pgBackRest from the real host

With `self-hosting/pgbackrest.env` filled in for R2 (mode 0600; Compose includes
it when it exists), run these from `self-hosting/`:

```sh
dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay check
dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay --type=full backup
dc exec -T -u postgres postgres pgbackrest --stanza=backendrelay info
```

`check` proves that the repository, the archive command and the encryption
passphrase work from inside the database container; the full backup proves an
object upload end to end. Confirm `status: ok` and the completed full backup in
`info`, that `WalArchiveFailing` and `BackupStale` stay quiet, and that the
healthchecks.io backup check was pinged by the schedule
([pgBackRest](../self-hosting/backup/README.md#one-time-setup)).

The local drill
([`pgbackrest-drill.sh`](../self-hosting/backup/pgbackrest-drill.sh)) uses a
`posix` repository on purpose: pgBackRest's S3 driver requires TLS, and a local
S3 emulator usually serves plain HTTP.

### Monitoring checklist

- [ ] The overlay and the canary profile are running, and `live-check.sh` passes.
- [ ] Grafana Explore shows recent container logs from the Loki datasource.
- [ ] UptimeRobot or Better Stack checks cover
      `https://api.<domain>/health/system`, the dashboard, and TLS expiry with at
      least 14 days' notice.
- [ ] healthchecks.io checks cover the `Watchdog` dead man and the backup ping.
- [ ] A critical test alert reached the phone and was acknowledged.
- [ ] Stopping Alertmanager for ten minutes paged through the dead man, and the
      restart restored the check.
- [ ] The three `pg_stat_archiver_*` series exist and the synthetic failure fired
      `WalArchiveFailing` to the phone.
- [ ] Archiving was broken once against a disposable archive target and the alert
      fired; production itself was not used for that test.
- [ ] `pgbackrest check` passed on the host and a full backup uploaded to R2.

## Data retention

Two background jobs bound table growth. Both defaults are deliberately
conservative and can be changed with environment variables; restart `serve`
after changing them.

| Data | Default | Setting | How it is deleted |
|---|---|---|---|
| Webhook/entity history partitions | 30 days | `INSTANT_HISTORY_RETENTION_DAYS` (minimum 1) | `history-truncator` truncates whole 30-day partitions outside the retained window, oldest first, together with the matching `webhook_events` partition. A value of `120` reproduces the previous 90–120 day window. |
| `transactions` marker rows | 30 days | `INSTANT_TRANSACTION_RETENTION_DAYS` (minimum 1) | The daily `hard-deletion-sweep` (10:00 US/Pacific) deletes rows older than the cutoff in `custodian-batch-size` batches, keeping the newest marker row for each app. |

Before upgrading from a release that kept 90–120 days, set
`INSTANT_HISTORY_RETENTION_DAYS=120` if that older history must survive: the
first run with the 30-day default truncates every older partition immediately.

`transactions` rows are safe to remove after the retention window because no
reader needs the full history:

- Realtime reads the logical WAL stream, not the table, and ignores delete
  transactions that do not start with a `transactions` insert.
- The startup watermark and the runtime sync snapshot read only `MAX(id)` per
  app or globally. The sweep keeps the newest row per app, so those watermarks
  never reset to zero for an idle app.
- `sync_snapshot` and the failover marker look up the newest or a freshly
  inserted row.
- The daily metrics worker reads rows created after the last date recorded in
  `daily_app_transactions`, which normally means the previous day. Keep the
  retention at least as long as the longest expected metrics outage (for
  example, a disabled worker or a restore); if metrics are off for longer than
  the window, the deleted days cannot be recomputed from `transactions`.

The custodian still deletes a whole app's `transactions` rows when the app is
hard-deleted; the retention sweep is additive to that path.

## WAL health and invalidation stream flapping

`GET /health/system` returns `200 {"wal":"ok"}` only when this process's
invalidation stream is healthy and no live process has published a `wal-errors`
marker. `500 {"wal":"error"}` means readiness is withheld; it is not by itself a
permanent failure.

`GET /health/version` is an unauthenticated build check, never a readiness
signal: it returns `{"version":"<tag>","git_sha":"<revision>","migration_version":<n>}`
for the running binary. Use `migration_version` to confirm the schema this
binary expects before running `migrate up`.

Readiness is withheld while the invalidator's logical replication connection is
down. Up to three producer restarts in a rolling ten-minute window clear as soon
as the stream reconnects. A fourth restart inside the window means the stream is
flapping: `{"wal":"error"}` and the `InvalidationStreamFlapping` alert stay
active until the oldest of those four restarts leaves the window (at most ten
minutes after the burst stops; restarts that keep coming keep it withheld). The
state is re-evaluated every five seconds while connected, so health clears by
itself: the process removes its own `wal-errors` entry and no restart or manual
marker cleanup is needed. Realtime keeps working while readiness is withheld;
the signal means "this stream was unstable, look at PostgreSQL". A
connection-stage failure before the first successful stream is not a producer
restart and does not hold readiness back.

Each stream failure logs a `warn` line (`invalidation stream failed;
reconnecting`, with `error_kind` and `retry_after_ms`) and each reconnect an
`info` line; `backendrelay_invalidation_stream_restarts_total` and
`backendrelay_invalidation_health` expose the same on `/metrics`.

## Logs

Containers use Docker's `local` log driver with a 50 MB file and five files per
service, so one noisy process cannot fill the host disk. The monitoring overlay
adds Loki and Grafana Alloy: Alloy reads container logs through a read-only
Docker API proxy (the `local` driver writes no tailable files), and Loki keeps
them for 14 days on a named volume. Grafana is provisioned with a Loki
datasource next to Prometheus. Realtime WAL refresh filtering is logged at
`debug`; the per-record line is not emitted at the default `info` level.

### Runtime log level

The process log filter can be replaced without a restart, so raising verbosity
does not drop live sockets or rotate the invalidation-slot identity. The filter
endpoint lives on the metrics listener, which binds to loopback by default:

- `GET /log-level` returns the current `EnvFilter` directive.
- `POST /log-level` takes a plain-text directive of at most 512 bytes, for
  example `curl -X POST --data 'debug,pg_walstream=warn' http://127.0.0.1:9091/log-level`.
  A valid directive returns `200` with the applied filter; an unparsable or
  oversized directive returns `400` and leaves the current filter unchanged.

## External monitoring and canary

Prometheus, Alertmanager and Grafana run inside the server's network. They
cannot see a dead host, an expired certificate or a broken public edge, so add
outside-in checks on top:

- **Uptime checks (owner).** Point UptimeRobot or Better Stack at
  `https://api.<domain>/health/system`, at the dashboard URL, and at the API
  certificate expiry. The TLS check must alert at least 14 days before
  expiration. `/health/system` covers readiness only; the dashboard check
  catches a broken dashboard image or edge route.
- **Dead-man checks (owner).** Point healthchecks.io at the Alertmanager
  `Watchdog` webhook (the URL inside `DEADMAN_WEBHOOK_FILE`) and at the backup
  job's `PGBACKREST_HEALTHCHECK_URL`. Give each check a period slightly longer
  than its normal cadence: the Watchdog fires on every evaluation interval and
  the backup timer runs daily.

### Canary

The canary container opens a realtime subscription to a dedicated app, writes
one `canary` row every `CANARY_INTERVAL_SECONDS` (default 60) through the admin
API, waits for the subscribed refresh carrying that row's id, and records the
write-to-refresh time. Each probe is bounded by `CANARY_TIMEOUT_SECONDS`
(default 10). It keeps only the newest 100 rows. A successful probe also pings
`CANARY_HEALTHCHECK_URL`; a failed probe does not, so healthchecks.io alerts
after its grace period.

Create the app and configure the canary:

1. Create a new app in the dashboard. Copy its app id and admin token; do not
   reuse an application's credentials.
2. Write the admin token to a root-owned, mode-600 file and export its absolute
   path as `CANARY_ADMIN_TOKEN_FILE`. The container reads the token from a
   read-only secret mount; the token is never passed in the environment.
3. Set `CANARY_API_URL` to the public API origin, for example
   `https://api.example.com`, and `CANARY_APP_ID` to the new app id.
4. Set `CANARY_HEALTHCHECK_URL` when the healthchecks.io check exists, then
   start the profile with the monitoring overlay:
   `docker compose -f docker-compose.yml -f docker-compose.monitoring.yml --profile canary up -d canary`.
5. Confirm `canary_probe` success lines in `docker compose logs canary` and the
   `backendrelay_canary_propagation_seconds` sample in Prometheus.

Prometheus scrapes `canary:9465` and evaluates `CanaryFailing` (no success for
five minutes) and `CanaryPropagationSlow` (p95 write-to-refresh above five
seconds for ten minutes). The canary target is part of the monitoring config,
so enabling the overlay without the `canary` profile makes
`ServiceUnavailable` report the missing target; enable both together.

## Error tracking

D11 selects Bugsink, a self-hosted Sentry-protocol server, for crash and error
reporting. It is internal-only: the `bugsink` service lives in the monitoring
overlay behind the `errors` profile, binds `127.0.0.1:8000`, and stores SQLite
in the `bugsink_data` volume. Bugsink is PolyForm Shield licensed, which is fine
for internal use.

Setup on the host:

1. Choose a reviewed Bugsink image and set `BUGSINK_IMAGE` to its digest-pinned
   reference.
2. Create the secret file with one line, `SECRET_KEY=<long random value>`, and
   export its absolute path as `BUGSINK_SECRET_KEY_FILE`.
3. Set `BUGSINK_BASE_URL` to the URL operators will use to reach it, for
   example `https://bugsink.<tailnet>.ts.net`.
4. Start it with the monitoring overlay and the `errors` profile, for example
   `docker compose -f docker-compose.yml -f docker-compose.monitoring.yml --profile errors up -d bugsink`.
5. Reach the UI through `tailscale serve` or an SSH tunnel. Do not publish the
   port directly; if it must be public, put Cloudflare Access in front of it.
6. Sign in, create a project, and copy its DSN (project settings → client keys).
7. Point both producers at that DSN:
   - Server: set `SENTRY_DSN` and, optionally, `SENTRY_ENVIRONMENT`. When
     Bugsink runs on the same compose network, rewrite the DSN host to
     `http://bugsink:8000/<project-id>` so the server does not depend on
     Tailscale. Restart the server. It reports panics and `error!` events,
     sends `warn!` events as breadcrumbs, and flushes for up to two seconds on
     shutdown.
   - Dashboard: set `NEXT_PUBLIC_SENTRY_DSN` on the `www` service to the
     tailnet-facing DSN and restart it. The launcher writes the value into
     `publicSelfHostedVariables.js`, so the image does not need rebuilding.
     Session replay and tracing stay off.
8. Trigger one deliberate failure on each producer and confirm both events
   arrive with no request body, cookies, headers or query string.

The DSN is a public ingestion identifier, not a credential; keep the Bugsink
`SECRET_KEY` private. Reporting is off whenever the DSN is unset or blank.
Bugsink has no built-in retention limit, so prune old projects and events in
the UI when the volume grows.

## Key rotation

`instant-server` reads three independent secrets from its environment (or from
`$INSTANT_CONFIG_DIR/secrets.env`):

| Variable | Purpose |
| --- | --- |
| `INSTANT_AEAD_KEYSET_JSON` | Tink AES-GCM keyset that encrypts stored secrets such as app OAuth client secrets and BYOP connection strings. |
| `INSTANT_WEBHOOK_KEYSET_JSON` | Tink Ed25519 keyset that signs webhook payloads and delivery signatures. |
| `INSTANT_RATE_LIMIT_HMAC_KEY` | HMAC key that maps rate-limit identities to bucket IDs. |

The Tink keysets support several enabled keys. The runtime encrypts and signs
with the key named by `primaryKeyId` and decrypts/verifies with any other
enabled key in the set, so rotation is a two-step process: publish the new
primary key first, then re-encrypt old rows.

### Rotate the AEAD and webhook keysets

1. Back up the current secrets file and the database. Run
   [`secrets-backup.sh`](../self-hosting/backup/secrets-backup.sh) as described
   under [Back up `server_secrets`](#back-up-server_secrets) so the rotation
   has a verified age-encrypted copy in R2 and offline. Every key still needed
   for decryption lives in the secrets file; losing it makes stored secrets
   unreadable.
2. Generate a rotated file to a new path. The command never edits the input
   file:

   ```sh
   instant-server generate-secrets --rotate /home/instant/config/secrets.env \
     > /home/instant/config/secrets.env.new
   ```

   The output adds a freshly generated key to both keysets, makes it the
   `primaryKeyId`, and keeps every existing key `ENABLED` for decryption and
   signature verification. All other lines, including
   `INSTANT_RATE_LIMIT_HMAC_KEY`, are copied unchanged.
3. Inspect the new file, then replace the old one and restart the server:

   ```sh
   mv /home/instant/config/secrets.env.new /home/instant/config/secrets.env
   ```

   New writes immediately use the new primary key. Old ciphertexts keep
   decrypting because their keys remain in the set.
4. Re-encrypt existing ciphertexts so a later rotation can drop the old key:

   ```sh
   instant-server reencrypt --dry-run   # report counts only
   instant-server reencrypt
   ```

   `reencrypt` walks every ciphertext produced with the AEAD keyset, re-encrypts
   rows whose key ID is not the current primary, and commits 500 rows per
   transaction. It prints scanned and re-encrypted counts per table:

   - `apps.connection_string` — encrypted BYOP database URLs.
   - `triples.$oauthClients.encryptedClientSecret` — encrypted OAuth client
     secrets for app sign-in.

   The command is idempotent: a second run reports zero re-encrypted rows. Run
   it during low traffic and keep serving; existing readers keep working while
   rows move to the new key.
5. After `reencrypt` reports zero pending rows and the backup includes the new
   secrets file, a **later** rotation may remove the old key. Until then, keep
   every key in the set. Removing a key that still protects a row makes that
   row unreadable.

### Rotate the webhook signing key

`generate-secrets --rotate` also adds a new Ed25519 webhook key and makes it
primary. Receivers verify signatures with the JWKS document at
`/.well-known/webhooks/jwks.json`, which publishes every enabled key in the
keyset, including the old one. Keep the old key enabled until webhook retries
and any in-flight payload tokens have expired (payload tokens are valid for one
hour, and delivery retries can extend that window), then drop it in a later
rotation.

### Rotate the rate-limit HMAC key

`generate-secrets --rotate` does not change `INSTANT_RATE_LIMIT_HMAC_KEY`.
Rotate it separately by replacing the value in the secrets file and restarting
the server.

The HMAC key only derives opaque rate-limit bucket IDs; it does not encrypt
stored user data. Changing it changes the derived bucket IDs, so existing
rate-limit counters are no longer matched and affected clients start with fresh
buckets. There is no data loss and no migration step. Rotate it during low
traffic so the reset does not coincide with a burst.

## Releases

Releases use date-based tags such as `v2026.11.1`. A tag is immutable: fixes go
into the next tag instead of moving an existing one.

1. Add the release's `## [vYYYY.MM.N]` section to `CHANGELOG.md`, merge it, and
   tag the commit that contains it:

   ```sh
   git tag v2026.11.1
   git push origin v2026.11.1
   ```

2. Wait for the tag's workflows. `release-images.yml` builds and signs the
   server and dashboard images for `linux/amd64` and `linux/arm64` and uploads
   `release-digests.json`. `nightly.yml` must also have passed on that commit;
   if it has not run there yet, start it for the tag:

   ```sh
   gh workflow run nightly.yml --ref v2026.11.1
   ```

3. From a checkout of the tag, run the release gate:

   ```sh
   git fetch --tags
   git checkout v2026.11.1
   make release-check TAG=v2026.11.1
   ```

   The gate checks, printing `check=<name> result=pass|fail` for each (the
   first release's upgrade/rollback check prints `skip`):

   - the tag exists and the latest nightly run for its commit succeeded;
   - `release-images.yml` published digests for the commit, and both images
     verify (multi-arch platforms, cosign signature against
     `self-hosting/cosign.pub`, SBOM and provenance);
   - the previous release upgrades and rolls back to this digest (skipped with
     a message for the first release);
   - the official SDK smoke suite passes against the exact server digest;
   - `CHANGELOG.md` has a `## [<tag>]` section.

   It exits non-zero and prints a final summary if any check fails. Live Docker
   and registry access is required, plus an authenticated `gh`, `jq`, `cosign`,
   and the release public key; set `COSIGN_PUBLIC_KEY` to use another trust
   anchor.

4. For a release that touches the realtime or query path, run the
   [weekly scale regression](#weekly-scale-regression) from a checkout of the tag
   and compare it with the committed baseline.

5. Deploy only after the gate passes. The deployment takes a pre-deployment
   backup and refuses to continue if that backup fails; see
   [Migrations and upgrades](self-hosting.md#migrations-and-upgrades) for the
   upgrade itself.

## Dependency updates

[`.github/dependabot.yml`](../.github/dependabot.yml) opens grouped dependency
PRs every Monday: Cargo in `server-rs/`, pnpm in `client/`, GitHub Actions pins,
the Dockerfiles, and the `self-hosting/` Compose stacks. Semver ecosystems
(Cargo, pnpm, Actions) keep majors in a separate PR so a breaking bump is never
mixed with routine updates, and the queue is capped at five PRs per ecosystem.
`cel` is ignored because the workspace patches it to `server-rs/vendor/cel`.

CI must pass before merging, and merges stay manual: there is no auto-merge
label or workflow. Review the changelog and lockfile diff, then merge and let
CI verify the commit like any other.

Monitoring images are supplied by the operator through `${PROMETHEUS_IMAGE}`,
`${ALERTMANAGER_IMAGE}`, `${GRAFANA_IMAGE}`, `${NODE_EXPORTER_IMAGE}` and
`${POSTGRES_EXPORTER_IMAGE}` in
`self-hosting/docker-compose.monitoring.yml`, so Dependabot cannot see them.
Review those digest-pinned images quarterly with
`docker buildx imagetools inspect <image>` and upgrade one at a time.

## Failure drills

## Drills

Three kinds of drill live here. The [failure drills](#failure-drills) inject one
fault into a running local stack and measure how fast the server recovers. The
[abuse and isolation drill](#abuse-and-isolation-drills) hammers one tenant on
staging and proves the limits hold without hurting its neighbor. The
[disaster-recovery drills](#disaster-recovery-drills) restore real backups into
staging or a lifeboat deployment and measure how long a full recovery takes.
Record every run: a drill that is not written down did not happen.

### Failure drills

`self-hosting/scale-local/drills.sh` injects one fault at a time into a running
scale-local stack (`stack.sh up`) and prints `drill=` lines with health, canary
and recovery times. Run each drill while `run.sh` drives load:

```sh
cd self-hosting/scale-local
bash stack.sh up
bash run.sh --apps 3 --users-per-app 200 --steady 3600 &
bash drills.sh kill9            # or walsender | pg-restart | invalidate-slot | disk-pressure
bash stack.sh down
```

Health means `/health/system` returns 200. The canary means PostgreSQL
accepts queries and an active `invalidator_*` slot has a usable WAL position.
The stack has no supervisor, so drills that kill the server restart the
recorded binary themselves; the times measure the server's own recovery, not a
supervisor's restart delay. Alerts are only checked when a Prometheus answers
at `SCALE_PROMETHEUS_URL`; scale-local runs none.

Results on 2026-10-27 (commit 78d891bc, 3 apps × 200 users of steady load,
desktop with SUT and load generator pinned to separate cores):

| Drill | Fault | Recovery | Notes |
| --- | --- | --- | --- |
| `kill9` | `SIGKILL` the server, restart it | 0.8 s | Health and canary 0.6–0.8 s after the restart. |
| `walsender` | `pg_terminate_backend` the invalidator walsender, 3 times | 0.7–1.4 s each | Each restart clears health on reconnect. |
| `walsender` (flap) | A 4th termination inside 10 minutes | Health withheld, then cleared by itself after 600 s | The stream stays connected and realtime keeps working; see [WAL health](#wal-health-and-invalidation-stream-flapping). |
| `pg-restart` | `pg_ctl restart -m fast` | 1.2 s | Pools replace the terminated connections; replication streams reconnect with backoff. |
| `invalidate-slot` | Cap `max_slot_wal_keep_size`, push WAL past it, restart the server | 0.8 s (0.6 s after restart) | The lost slot stays visible until the hourly replication-slot cleanup drops it. |
| `disk-pressure` | Server down 10 minutes while WAL grows under a 64 MB slot cap | 0.6 s after restart | Retained WAL stayed at 80 MiB; the slot was invalidated after about 4 minutes and the restarted server wrote a fresh one. |

The first runs found three bugs, fixed before these results:

- After a fourth invalidation-stream restart within ten minutes, health stayed
  at `{"wal":"error"}` for the life of the process (P3.12.f1).
- A fast PostgreSQL restart made the whole server exit, because a `FATAL`
  `57P03` during reconnect was treated as permanent (P3.12.f2).
- A fast PostgreSQL shutdown never finished while the sketch aggregator was
  connected. PostgreSQL waited for a confirmation the aggregator could not
  give, and the two spun on reply requests (P3.12.f3).

Client-visible error counts during each fault are not measured yet. The load
generator is stopped between runs rather than summarised.

### Abuse and isolation drills

P7.10 answers two questions on staging, on the production hardware: do the
public limits hold when one tenant misbehaves, and does a second tenant notice?
The drill creates throwaway apps A and B, measures B's read p95, then hammers A
from the desktop: magic-code sends, guest sign-ups, an oversize and an HTML
upload, expensive queries, and 100 live sessions from one IP. It runs over the
public path, so the per-IP limits resolve the desktop's real address through
`INSTANT_TRUSTED_PROXY_CIDRS`.

Credential isolation is the other half of TST-H3: run
`bash self-hosting/run-cross-app-isolation-acceptance.sh` as well. That suite
proves A's refresh token, admin token, SSE credentials, presigned URLs, rooms,
streams and webhook configuration cannot reach B; this drill proves A's traffic
cannot hurt B.

The limits under test and where they come from:

| Control | Setting (default) | What the drill expects |
| --- | --- | --- |
| Magic-code sends per (app, email) | `magic-code-rate-limit-per-hour` (20) | 20 accepted, then 429 `rate-limited`. |
| Magic-code sends per client IP | `magic-code-rate-limit-per-ip-hour` (30) | 30 accepted, then 429. |
| Magic-code sends per app per day | `magic-code-daily-limit-per-app` (1000) | Not reached from one IP; check the flag and the metric. |
| Pre-auth request body | 1 MiB for `/dash/*`, `/admin/*`, `/superadmin/*` | 1 MiB + 1 returns 413 before auth. |
| InstaQL query JSON | `max-instaql-query-bytes` (256 KiB) | An over-size `add-query` returns `parameter-limit-exceeded`. |
| Upload size | `INSTANT_STORAGE_MAX_UPLOAD_BYTES` (100 MiB; 1 MiB for the drill) | Cap + 1 returns 413. |
| Upload content headers | attachment except inline media | An HTML download is `attachment` and not `text/html`. |
| Live sessions per IP | `max-live-sessions-per-ip` (100) | 100 upgraded, then 429 `rate-limited`. |
| Interactive fairness | P3.5 pools, timeouts and per-app limits | A's queries stay bounded and B's p95 stays near its baseline. |

Prerequisites, the exact staging overrides and the per-check meanings are in
[`self-hosting/drills/abuse/README.md`](../self-hosting/drills/abuse/README.md).
At minimum: staging on the production tier, `INSTANT_EPHEMERAL_APPS=true`,
`INSTANT_TRUSTED_PROXY_CIDRS` set to the staging Docker subnet,
`INSTANT_STORAGE_MAX_UPLOAD_BYTES=1048576`, and no other traffic from the
desktop's address in the window (the live-session cap is process-wide). Unset a
config-app flag that overrides any of these
(`flags get max-live-sessions-per-ip`, `magic-code-rate-limit-per-ip-hour`,
`storage-quota-bytes`).

Run it from the desktop:

```sh
export ABUSE_API_ORIGIN=https://staging-api.<domain>
mise exec node@24 -- node self-hosting/drills/abuse/abuse-drill.mjs
```

Exit status `0` means every check passed, `1` a check failed, and `2` the run is
invalid. The driver prints the attack window and writes `result.json` under
`${XDG_STATE_HOME:-~/.local/state}/backendrelay-abuse/<stamp>/`; it holds no
tokens. A failed check is a finding: fix the limit or the proxy configuration
and rerun before recording a pass.

For the same `startedAt`..`endedAt` window, collect the evidence the driver
cannot:

1. **Alerts.** Confirm that the alerts for the saturated surface fired and write
   down their names. Staging normally has no Prometheus scrape, so either attach
   one for the window or record the alert check as not exercised; do not count a
   production dashboard as evidence for a staging run. The surfaces map to
   `AdmissionPressure`, `RequestLatency`, `RealtimeRefreshBacklog`,
   `RealtimeRefreshRejected`, `PoolPressure` and `WebsocketExitsHigh`.
2. **Email provider.** Count sends at the provider for the window. The count
   must not exceed the accepted magic-code sends (the driver's per-IP accepted
   count: 30 when fresh) and must never pass the daily quota. With a scraper
   attached, cross-check `backendrelay_emails_total{outcome="sent"}` against
   `{outcome="rate_limited"}`: rate-limited sends never reach the provider.
3. **Not exercised by one desktop.** The per-app daily quota, the storage quota,
   the 500-subscription cap and the per-app session cap are configuration
   checks plus their live tests.
   `statement_timeout`, `lock_timeout` and `idle_in_transaction_session_timeout`
   are per pool connection; the expensive-query ceiling and B's p95 are the
   observable side.

Delete apps A and B and restore the staging overrides when the run is recorded.

Results on the staging project (production hardware, separate database). Record
the date, the staging server digest and the commit with the copied rows:

| Check | Expected | Measured | Result | Notes |
| --- | --- | --- | --- | --- |
| Magic-code sends to one address | 20, then 429 | pending | pending | Fresh per-address bucket. |
| Magic-code sends from one IP | 30, then 429 | pending | pending | Fresh per-app bucket. |
| Pre-auth body | 413 | pending | pending | |
| InstaQL query size | `parameter-limit-exceeded` | pending | pending | |
| Oversize upload | 413 | pending | pending | 1 MiB + 1. |
| HTML upload | attachment, no `text/html` | pending | pending | |
| Guest sign-up burst | no 5xx; B unaffected | pending | pending | No dedicated quota. |
| Expensive queries | bounded; B p95 near baseline | pending | pending | |
| Sessions from one IP | 100, then 429 | pending | pending | |
| App B p95 | within 1.5x baseline or 250 ms | pending | pending | |
| App B read errors | no non-2xx reads | pending | pending | |
| Alerts | the saturated surface's alerts fired | pending | pending | Needs a staging scrape. |
| Email provider | no send past quota | pending | pending | Provider console. |

Set the target before the first run: every check passes, and app B's p95 stays
within 1.5x its own baseline or 250 ms, whichever is larger (the driver's
default). Rerun after changing a limit, a pool size, the trusted-proxy
configuration, or before a rollout wave.

### Disaster-recovery drills

P7.8 proves that the database backup, the `server_secrets` keysets and the
deployment kit work together, not just that a dump command exits zero. Run (a)
and (b) quarterly and (c) monthly (P8.4). Run them as well after changing
`recovery.py`, the PostgreSQL image or the deployment kit. Never run them
against production: use the staging project (P7.5) and a disposable lifeboat
deployment.

The measured time starts at the first step named in each drill and stops when
that drill's checks are green. Set the target for each drill before its first
run; a run over target is a finding, not a reason to change the target
afterwards.

| Drill | Date | Recovery | Result | Notes |
| --- | --- | --- | --- | --- |
| a | not run | — | pending | Latest backup restored into staging; apps, files and auth verified. |
| b | not run | — | pending | Lifeboat on Fly.io Frankfurt or Railway against a restored database. |
| c | not run | — | pending | One app restored from its per-app backup; other apps untouched. |

Owner targets, filled in before the first run: (a) `___` minutes, (b) `___`
minutes, (c) `___` minutes.

Append a row with
[`record-drill.sh`](../self-hosting/drills/record-drill.sh). The full command
sequences, placeholders and teardown steps are in
[`self-hosting/drills/README.md`](../self-hosting/drills/README.md); the lifeboat
template is
[`self-hosting/drills/lifeboat/fly.toml.example`](../self-hosting/drills/lifeboat/fly.toml.example).

#### Prerequisites

- A verified bundle in `/var/lib/backendrelay/backups`: its receipt must say
  `offhostVerified: true`, and its `sha256` must match the local file. Fetch the
  off-host copy if the local one has aged out. Keep the age identity offline.
- The staging project from P7.5 on the production host, with its own buckets,
  keysets and hostnames. Restoring production keys into staging is acceptable
  for the drill only; destroy them when the drill ends.
- A lifeboat account (Fly.io Frankfurt or Railway), a `lifeboat-api.<domain>`
  test hostname, and a database copy or managed PostgreSQL endpoint for the
  lifeboat. The same signed server digest as production.
- At least 30 GiB free in `/srv/drills`, and `age`, `jq` and `curl` on the host.
- For files, either a read-only token scoped to the production files bucket or a
  copy of the objects. Restored `$files` rows point at production keys, so the
  staging bucket does not serve them by itself.

#### (a) Restore the latest backup into staging

1. Record the start time and stop staging traffic. This replaces the staging
   database and keysets; production is untouched.

   ```sh
   dcs() { DEPLOY_DB_SERVICE=aux-postgres docker compose \
     --project-name backendrelay-staging --project-directory . \
     --env-file deploy/.env.staging --profile staging \
     -f deploy/docker-compose.yml "$@"; }
   dcs stop caddy dashboard server aux-postgres
   dcs rm -f caddy dashboard server aux-postgres
   docker volume rm backendrelay-staging_aux-db
   docker volume create backendrelay-staging_aux-db
   ```

2. Decrypt the newest bundle into the scratch directory and confirm the receipt.
   Never decrypt into the repository or a world-readable directory.

   ```sh
   sudo install -d -m 700 /srv/drills/restore
   sudo jq -r '.offhostVerified, .completedAt, .sha256' \
     /var/lib/backendrelay/backups/<stamp>.receipt.json
   sudo sha256sum /var/lib/backendrelay/backups/<stamp>.tar.age
   sudo age --decrypt -i /media/offline/backendrelay-age.key \
     -o /srv/drills/restore/bundle.tar /var/lib/backendrelay/backups/<stamp>.tar.age
   sudo tar -xf /srv/drills/restore/bundle.tar -C /srv/drills/restore
   ```

3. Start an empty database on the staging volume and load `primary.dump`. The
   `--no-owner --no-acl` restore leaves the objects owned by `postgres`, so grant
   the staging role access before the server connects as it.

   ```sh
   dcs run -d --name backendrelay-restore-staging --no-deps aux-postgres
   until docker exec backendrelay-restore-staging pg_isready -U postgres; do
     sleep 1
   done
   docker exec -i backendrelay-restore-staging sh -ec \
     'exec pg_restore --exit-on-error --no-owner --no-acl \
       -U "$POSTGRES_USER" -d "$POSTGRES_DB"' \
     < /srv/drills/restore/primary.dump
   ```

   Grant the role named by `AUX_DB_USER` in `deploy/.env.staging` (the default
   example is `staging`) on the database named by `AUX_POSTGRES_DB`:

   ```sh
   docker exec -i backendrelay-restore-staging \
     psql -U postgres -d <aux-postgres-db> \
     -c 'GRANT USAGE ON SCHEMA public TO "<aux-db-user>";' \
     -c 'GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON ALL TABLES IN SCHEMA public TO "<aux-db-user>";' \
     -c 'GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO "<aux-db-user>";'
   ```

4. Replace the staging keysets with the restored `server-secrets.tar`. The
   archive's root is the config directory, so extract it at the volume root.
   `configuration.tar` is not used here: staging keeps its own deployment files.

   ```sh
   docker rm -f backendrelay-restore-staging
   docker volume rm backendrelay-staging_server_secrets
   docker volume create backendrelay-staging_server_secrets
   mount=$(docker volume inspect --format '{{.Mountpoint}}' \
     backendrelay-staging_server_secrets)
   sudo tar -xf /srv/drills/restore/server-secrets.tar -C "$mount"
   sudo chown -R 10001:10001 "$mount"
   sudo chmod 700 "$mount"
   sudo chmod 600 "$mount/secrets.env"
   ```

5. Start staging and check health, then verify apps, files and auth through the
   public staging hostnames.

   ```sh
   dcs up -d --no-deps aux-postgres server dashboard caddy
   curl -fsS https://staging-api.<domain>/health/system
   ```

   - **Apps:** sign in at `https://staging-dash.<domain>` and compare the app
     list and a few row counts with production.
   - **Files:** set `S3_FILES_BUCKET=instantdb-files` and a read-only production
     token in `deploy/.env.staging` for the drill, recreate `server`, and
     download a file from a restored app. Revert both values afterwards; do not
     run a normal staging deploy while they point at production.
   - **Auth:** request a magic code for the owner address, complete the sign-in,
     and open an app whose OAuth client secret was stored before the backup. If
     staging has no email provider, the code is logged at WARN instead of sent;
     read it from the server log and still complete the sign-in.

6. Record the elapsed time from step 1, then destroy the production copy in
   staging. Leaving production keysets or data there is a finding on its own.

   ```sh
   sudo bash self-hosting/drills/record-drill.sh --drill a \
     --date "$(date -u +%F)" --seconds <elapsed> \
     --notes "bundle <stamp>; apps/files/auth verified"
   dcs stop caddy dashboard server aux-postgres
   dcs rm -f caddy dashboard server aux-postgres
   docker volume rm backendrelay-staging_aux-db backendrelay-staging_server_secrets
   ```

   Recreate staging's empty database and staging-only keysets with the one-time
   setup in [`deploy/README.md`](../self-hosting/deploy/README.md#one-time-staging-setup)
   before using it for a release rehearsal again.

#### (b) The lifeboat

The lifeboat is the same signed server image running somewhere other than the
production host, against a copy of the database, with the production
`server_secrets` restored. It proves the deployment is portable and that the
keysets are enough to serve existing app identities.

1. Restore the database copy. For a logical bundle, load `primary.dump` into the
   lifeboat PostgreSQL with `pg_restore --exit-on-error --no-owner --no-acl`,
   then grant the application role as in drill (a). If the lifeboat uses managed
   PostgreSQL with point-in-time recovery, restore to the newest recoverable
   time instead and run `instant-server migrate up` with the same binary.
2. Restore `server_secrets` into a volume mounted at `/home/instant/config` on
   the lifeboat, or provide the three keyset variables as platform secrets. Do
   not generate new keysets: new keys cannot decrypt existing rows.
3. Deploy the signed server digest with
   [`fly.toml.example`](../self-hosting/drills/lifeboat/fly.toml.example) or the
   equivalent Railway service. Replace the app name, image digest and
   `INSTANT_BACKEND_URL`; import the rest of the production server environment
   with the database URL and hostnames changed.
4. Point `lifeboat-api.<domain>` at the deployment and confirm health, an app
   query with a real admin token, a file download and a magic-code sign-in.
   Realtime is optional for the drill, but a WebSocket write and refresh is the
   strongest proof.
5. Record the elapsed time and tear the lifeboat down completely: destroy the
   app, its volume and the database copy, then revoke any tokens created for it.
   A lifeboat left running is a second production copy with production keys.

#### (c) Restore one app from its per-app backup

Restores create a **new** app; they never overwrite an existing one. That makes
this drill the isolation check: the restored copy must be complete and every
other app must keep serving.

1. In the dashboard **Backups** tab, create a fresh backup of one app and
   download the zip.
2. Start a restore to a new app. Use a dashboard admin session token; keep it in
   an environment variable and never in shell history.

   ```sh
   curl -fsS -X POST \
     "https://api.<domain>/dash/restores/zip?title=Restore+drill" \
     -H "Authorization: Bearer $INSTANT_DASHBOARD_TOKEN" \
     -H 'Content-Type: application/zip' \
     --data-binary @instant-backup-<stamp>.zip
   ```

   The response contains a `restore-job` with an `id` and the new `app_id`.
3. Poll the job until `job_status` is `done` (or `error`):

   ```sh
   curl -fsS "https://api.<domain>/dash/restore-jobs/<job-id>" \
     -H "Authorization: Bearer $INSTANT_DASHBOARD_TOKEN"
   ```

4. Open the restored app, check its schema, a few entities and a file download.
   In the original apps, run the canary query and confirm they still serve; a
   restore job must not lock or rewrite other apps.
5. Record the elapsed time from starting the backup to the restored app opening,
   then delete the restored drill app and its backup.

## Onboarding an app (P8.3)

Work through this list before an app sends production traffic. It assumes the
app exists in the dashboard, has an Owner member, and that the owner has stored
its app ID and admin token outside the repository. Keep the completed list with
the app's handover notes, and repeat the permissions, limits, auth and storage
checks after a material change.

The `kit` helper is the one defined in
[Pin the initial config flags](#pin-the-initial-config-flags).

### Permissions

- [ ] Rules are pushed and reviewed, `$default` denies everything, and
      `instant-server audit` reports no finding for the app.

Push the app's `instant.perms.ts` with `instant-cli push perms`, pointing the
CLI at this server with `INSTANT_CLI_API_URI=https://api.<domain>` (or `apiURI`
in `instant.config.ts`). You can also edit the **Permissions** tab in the
dashboard. Read the resulting document, start from a deny-all default, and grant
only the actions each entity needs:

```json
{
  "$default": { "allow": { "$default": "false" } }
}
```

Run the audit and keep only the lines for this app:

```sh
kit exec -T server /usr/local/bin/instant-server audit \
  | jq -c 'select(.app_id == "<app-id>")'
```

No output means no findings. `app-no-rules` and `app-empty-rules` are warnings,
but both mean the app is not ready; `app-default-allows` is an error and fails
the deploy. The audit also warns with `app-no-owner` when the app has neither a
creator nor an Owner member. The command audits every app, so a finding for
another app also makes it exit 1; the filter shows only this app's lines. See
[Run the audit](#run-the-audit) for the summary line and exit codes.

### Limits

- [ ] Magic-code limits cover the app's expected sign-in volume.
- [ ] Rule-level `$rateLimits` protect the expensive entities.
- [ ] Storage, session, refresh-waiter and cache quotas are sized for the app.

The magic-code limits are deployment-wide, so size them for the busiest app.
Read the values that are set (unset keys use the defaults below):

```sh
kit run --rm --no-deps -T server flags list | grep '^magic-code-'
```

The defaults are 20 sends per email address per hour, 1000 sends per app per
day, 30 sends per client IP per hour and 60 verifies per client IP per hour.
Raise the daily cap only when an app legitimately exceeds it; the per-IP limits
are the first line of defence against abuse. Over-quota sends return the same
rate-limited error as the per-email limit.

Add a `$rateLimits` bucket to the app's rules and call it from the actions that
can be abused:

```json
{
  "$rateLimits": {
    "writes": {
      "limits": [{ "capacity": 120, "refill": { "period": "1 minute" } }]
    }
  },
  "todos": {
    "allow": { "create": "rateLimit.writes.limit(auth.id)" }
  }
}
```

`capacity` is the burst size; `refill.period` accepts durations from one second
to 24 hours and defaults to one hour. Key on `auth.id` for per-user limits or on
a literal such as `'global'` for an app-wide limit.

Size the per-app quotas and set them through the config app:

```sh
# Soft storage quota for this app, in bytes (10 GiB here).
kit run --rm --no-deps -T server flags set storage-quota-bytes-by-app \
  '{"<app-id>": 10737418240}'

# Concurrent realtime sessions for one app; read at startup.
kit run --rm --no-deps -T server flags set max-sessions-per-app 5000
```

`INSTANT_REFRESH_WAITERS_PER_APP` (64 by default) caps queued refreshes per app;
change it in the deployment environment and restart. The per-app interactive
database share applies automatically. The `datalog-cache-max-weight` flag sets
the per-app result-cache budget and `INSTANT_RESULT_CACHE_MAX_BYTES` caps the
total; lower them if one app's large result sets crowd out the others. Restart
the server after `max-sessions-per-app`, then confirm the values with
`flags list`.

### Email

- [ ] The app sends from the verified BackendRelay sender, or its own domain is
      enabled for Cloudflare Email Sending and verified in the dashboard.
- [ ] SPF, DKIM and DMARC pass for the sender domain.

An app that keeps the default sender needs no per-app email work: confirm that
`INSTANT_APP_EMAIL_SENDER_EMAIL` is on the platform sender domain enabled in
P7.3. For an app with its own domain, follow
[Onboarding a per-app sender domain (Cloudflare)](self-hosting.md#onboarding-a-per-app-sender-domain-cloudflare).
Until Cloudflare reports the domain as enabled and the app completes the
sender-verification flow, mail falls back to the platform sender.

Send a real magic code through the app to a mailbox that shows authentication
results (or to a test address you control) and inspect the headers: `spf=pass`,
`dkim=pass` and `dmarc=pass` must all be present for the app's From domain. Fix
missing DNS records in Cloudflare before onboarding users; the per-app daily
send quota from [Limits](#limits) applies to these sends too.

### Auth

- [ ] Every OAuth client has explicit audiences for the platforms it serves.
- [ ] Every redirect origin is listed and exact.

Open the app's **Auth** tab. Each OAuth client always accepts its own
`client_id`; add native identifiers such as the iOS bundle ID or Android client
ID as extra audiences (`extra_audiences`, at most 20 entries of 256 characters
each). A token whose `aud` matches neither the client ID nor an extra audience
is rejected, including for clients without a secret.

Under **Authorized origins**, list the exact origins the app redirects to. A
generic origin is a host with an optional port; use `https` everywhere except
`localhost` and `127.0.0.1`. Do not use paths or wildcards. Vercel, Netlify and
custom URL schemes have their own origin kinds; enter the project or site name
rather than a full URL. Review the list after every preview-deployment or
bundle-ID change.

### Storage

- [ ] The app's web origin is in the files bucket CORS policy.

Browsers fetch uploads and presigned downloads directly from R2, so add the
app's origin to the **files** bucket CORS policy while keeping the dashboard
origin. In the Cloudflare dashboard: **R2 → instantdb-files → Settings → CORS**.
The policy is replaced as a whole, so keep every origin in the list.

[`r2-wrangler.sh configure-cors`](../self-hosting/r2-wrangler.sh) replaces the
bucket policy with `r2-cors.json`, substituting the origin you pass for
`ORIGIN_PLACEHOLDER`; it does not append. Keep every origin in that file before
running it, or apply a complete policy another way. Verify the applied list
with:

```sh
npx wrangler r2 bucket cors list instantdb-files
```

The app needs `GET`, `HEAD`, `PUT` and `DELETE`, the `Content-Type`, `If-Match`
and `If-None-Match` request headers, and the `ETag` and `Content-Encoding`
exposed headers, as in [`r2-cors.json`](../self-hosting/r2-cors.json).

### SDK

- [ ] The app pins an exact published SDK version.
- [ ] The app sets `apiURI` to the API origin.

Install the published BackendRelay SDK package and pin it without a range
operator. For example, pin `"@backendrelay/react": "1.1.0"` rather than
`"^1.1.0"`, and commit the lockfile. Set `apiURI` to the public API origin:

```ts
const db = init({
  appId: '<app-id>',
  apiURI: 'https://api.<domain>',
});
```

Set `websocketURI` explicitly only when the pinned SDK version does not derive
it from `apiURI`. Never point an app at `api.instantdb.com`; the server has no
default upstream URL. The `backendrelay_sdk_sessions` metric shows which SDK
versions are connected after the app's first session.

### Observability

- [ ] The app has a metric slug and appears in the per-app panels.

Read the current map, add this app without dropping the others, and restart the
server so the route labels pick it up:

```sh
kit run --rm --no-deps -T server flags list | grep '^app-metric-slugs'
kit run --rm --no-deps -T server flags set app-metric-slugs \
  '{"<app-id>":"<app-slug>","<other-app-id>":"<other-slug>"}'
kit up -d --no-deps --force-recreate server
```

The slug is lowercase letters, digits and `-`, at most 32 bytes, and never
`other`. An app without a slug uses the first eight characters of its UUID; only
the first 100 resolved apps keep their own label, and the rest report as
`other`. Confirm the label after the app has served traffic:

```sh
prom 'backendrelay_app_sessions{app="<app-slug>"}'
prom 'sum by (app) (rate(backendrelay_app_ops_total[5m]))'
```

The slug must also appear in the **Per app** row of the Grafana dashboard, next
to its sessions, operations, database time, errors, writes, triples bytes and
webhook queues.

### Backups

- [ ] The app is included in the nightly per-app backup.

The nightly worker runs at 20:00 US/Pacific when the deployment uses the
`production` profile or sets `INSTANT_NIGHTLY_APP_BACKUPS=true`. An app is
included unless it is soft-deleted, was created by the ephemeral-apps service
account, or is listed in the skip flag:

```sh
kit run --rm --no-deps -T server flags list | grep '^backup-skip-app-ids'
```

After the next run, open the app's **Backups** tab and confirm a backup with a
future expiry. To check from the database:

```sh
kit exec -T -u postgres postgres psql -U postgres -d instant \
  -c "SELECT backup_at, expires_at FROM app_backups
      WHERE app_id = '<app-id>' ORDER BY backup_at DESC LIMIT 1;"
```

Scheduled backups carry a seven-day retention; the bucket lifecycle rule from
P3.8 is the backstop. Restores create a new app, never overwrite this one; drill
(c) in [Restore one app from its per-app backup](#c-restore-one-app-from-its-per-app-backup)
exercises that path.

## Weekly scale regression

`self-hosting/scale-local/weekly.sh` runs the two weekly shapes (10 apps × 1,000
users and 50 apps × 100 users) on a disposable, CPU-pinned stack, keeps each
`result.json`, and compares the worst-phase p95 operation latency and the server
and PostgreSQL CPU-ms per operation with
[`self-hosting/scale-local/baseline.json`](../self-hosting/scale-local/baseline.json).
It fails when a run reports an error or any compared metric is more than 20%
above the baseline.

```sh
bash self-hosting/scale-local/weekly.sh
```

The desktop that runs the benchmark installs it as a **user** timer (nothing
installs it from the repository); see
[scale-local/README.md](../self-hosting/scale-local/README.md#desktop-timer):

```sh
cp self-hosting/scale-local/systemd/backendrelay-weekly-scale.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now backendrelay-weekly-scale.timer
```

It runs Sundays at 04:00 and catches up after a missed window. Run it by hand
before a release that touches the realtime or query path, and refresh the
baseline with `weekly_compare.py --update-baseline` after a deliberate change in
cost.

## Operating cadence (P8.4)

One person runs this deployment on four rhythms and one release path. Times are
UTC, and the `dc`, `sql` and `prom` helpers are the ones defined at the top of
this page. Record what you find: a drill result goes into [Drills](#drills), and
anything else becomes a fix in the same week rather than a note.

### Daily (5 minutes)

Read what arrived since the last check, then confirm nothing is still open:

1. **The alert digest.** Critical alerts repeat hourly until acknowledged;
   warnings group by `alertname` and repeat every four hours. Every one has a
   playbook under [Alert playbooks](#alert-playbooks).
2. **What is still firing:**

   ```sh
   dc exec -T alertmanager amtool --alertmanager.url=http://localhost:9093 alert query
   ```

3. **Backup age.** `prom 'backendrelay_backup_snapshot_seconds'` must stay under
   86400; `BackupStale` pages after 24 hours without a verified off-host copy.
4. **Canary.** `prom 'backendrelay_canary_last_success_timestamp_seconds'`
   proves the public edge, TLS and one real write-to-refresh path still work;
   `CanaryFailing` fires after five quiet minutes.

An alert the playbook already explained is a glance. One that is not explained is
today's work: fix it, or write it down before closing the laptop.

### Weekly

- **Merge dependency PRs once CI is green.** Renovate opens grouped PRs for
  Cargo, pnpm, Docker digest and GitHub Actions updates (P5.6). Read the failing
  job on a red PR and fix the pin; do not force it through, and keep a major
  upgrade out of a release week.
- **Read the per-app panels.** The Grafana **Per app** row separates realtime
  sessions, operations, database time, errors, writes, triples bytes and webhook
  queue depth by app; [Which app is hurting us](#which-app-is-hurting-us) has
  the same numbers as queries. Look for one app's database time or error rate
  rising while the others are flat, and check it against the quotas it was
  onboarded with ([Onboarding an app](#onboarding-an-app-p83)).
- **Check SDK versions.**
  `prom 'sum by (sdk, version) (backendrelay_sdk_sessions)'` shows the SDK family
  and version of every live session. A session on a version older than the one
  its app pins means a stale client deploy; `other` also collects every version
  past the top 20, so read it with the app list.
- **Read the weekly audit.** `backendrelay-deploy-audit.timer` runs
  `instant-server audit` every Monday from 06:00:

  ```sh
  systemctl status backendrelay-deploy-audit.service
  sudo journalctl -u backendrelay-deploy-audit.service --since '8 days ago'
  ```

  Findings are JSON lines (`app-no-rules`, `app-default-allows`,
  `app-no-owner`). Fix them this week, then rerun [the audit](#run-the-audit) by
  hand and confirm the summary line reports zero errors.

### Monthly

- **Restore one app in staging.** Drill (c) in
  [Disaster-recovery drills](#disaster-recovery-drills) restores one app from
  its per-app backup and proves the other apps are untouched. Use a different
  app each month and append the measured time with
  [`record-drill.sh`](../self-hosting/drills/record-drill.sh).
- **Run `make audit` and patch.** From the repository root it runs
  `cargo deny check advisories licenses sources` and `pnpm audit --prod`, with
  the exemptions in `server-rs/deny.toml` (reason and revisit date on every
  entry) and `pnpm.auditConfig.ignoreGhsas` in `client/package.json`. Patch what
  it reports or re-check the exemption, and remember the nightly CI `audit` job
  runs the same command.
- **Review sizing.** Compare the month's CPU, memory, connections and realtime
  p95 with [Sizing](performance.md#sizing) and the P7.9 baseline. The Phase 9
  triggers decide the next move: resize the CAX41
  ([Resize the CAX41](#resize-the-cax41)) when a container limit is close at the
  measured user count, add the second server replica when realtime p95 stays
  over 1 s with several apps active, and move to managed PostgreSQL
  ([managed-postgres.md](managed-postgres.md)) when provider failover becomes a
  requirement. Resize after a fresh baseline, never on a guess.
- **Prove the secrets copy.** Follow
  [Verify the off-host copy](#verify-the-off-host-copy): decrypt the newest
  `server_secrets` backup, compare it with the live file, and take a fresh copy
  when a rotation happened since.

### Quarterly

- **Run the full disaster-recovery drill, including the lifeboat.** Drills (a)
  and (b) in [Disaster-recovery drills](#disaster-recovery-drills) restore the
  latest verified backup into staging and stand the same signed server digest up
  on Fly.io Frankfurt or Railway against a copy of the database and the
  production `server_secrets`. Book the window and the teardown: an abandoned
  lifeboat is a second production copy holding production keys. The drill
  section says when a rerun is due, and a run that misses its target is a
  finding rather than a reason to move the target.
- **Check key rotation.** Read [Key rotation](#key-rotation) and decide whether
  the AEAD keyset, the webhook signing key or the rate-limit HMAC key is due:
  after a staff change, after a suspected exposure, or on your own interval.
  Rotation is two steps, publish the new primary key first and re-encrypt old
  rows after, so take and verify a fresh `server_secrets` backup before starting.
- **Review dependency major versions.** List the dependencies that are a major
  version behind in `server-rs/Cargo.toml` (including the patched `cel` under
  `server-rs/vendor/cel`) and in `client/package.json`. Upgrade one at a time,
  rehearse each on staging, and keep it out of a release that also changes the
  schema.
- **Watch upstream InstantDB until 2027-08-31.** The nightly `upstream-watch`
  job records the published `@instantdb/core` dist-tags in its run summary; read
  it, and check the upstream repository for SDK or security changes that affect
  the version your apps pin. After that date this fork stands on its own and the
  check stops.

### Release process

1. **Tag and build.** Tag the release and push the tag; the tag-triggered image
   workflow (P5.3) builds the multi-arch server and dashboard images, signs them
   with cosign keyless and attaches an SBOM. If a tag build is not available,
   build both by hand as in [Building images](self-hosting.md#building-images)
   and sign them with `cosign sign` before pushing. Verify what you are about to
   deploy, from the host or your workstation:

   ```sh
   git tag -a v2026.11.1 -m 'BackendRelay v2026.11.1'
   git push origin v2026.11.1
   bash self-hosting/verify-release-images.sh \
     <registry>/backendrelay/server@sha256:<digest> \
     <registry>/backendrelay/dashboard@sha256:<digest>
   ```

2. **Rehearse on staging.** Deploy the same digests with the staging env file;
   `deploy.sh` runs the canary app probe against the staging origin as part of
   the smoke test:

   ```sh
   cd /opt/backendrelay/self-hosting
   DEPLOY_ENV_FILE=deploy/.env.staging bash deploy/deploy.sh \
     <server>@sha256:<digest> <dashboard>@sha256:<digest>
   ```

   When the release carries a migration, rehearse one rollback there as well:
   `DEPLOY_ENV_FILE=deploy/.env.staging bash deploy/rollback.sh`. For a
   dashboard-only change,
   `INSTANT_DASHBOARD_IMAGE=<dashboard>@sha256:<digest> bash self-hosting/run-dashboard-image-canary.sh`
   proves the image is non-root and fails closed without runtime configuration.

3. **Deploy to production.** From the same directory, with the production env
   file, run the kit and keep its output:

   ```sh
   bash deploy/deploy.sh <server>@sha256:<digest> <dashboard>@sha256:<digest>
   ```

   `deploy.sh` verifies the signatures, takes a backup that must verify,
   migrates, recreates one replica at a time, smoke-tests the public edge, the
   canary query and the audit, and rolls back on its own when a step fails.

4. **Watch for 30 minutes.** Keep the dashboard and these three queries in front
   of you:

   ```sh
   prom 'sum by (surface) (rate(backendrelay_http_requests_total{status_class="5xx"}[5m]))'
   prom 'histogram_quantile(0.95, sum(rate(backendrelay_canary_propagation_seconds_bucket[10m])))'
   prom 'topk(10, sum by (app) (rate(backendrelay_app_errors_total[5m])))'
   ```

   Read the **Request errors**, **Realtime refresh waiters**, **Pool pending
   acquisitions** and **Per-app errors** panels with them, and scan
   `dc logs --since 30m server` for panics and error lines. The release is done
   when canary propagation p95 is under one second, no new alert fires, and
   `current.env` holds the digests you deployed.

5. **Roll back if it is wrong.** `bash deploy/rollback.sh` returns to
   `previous.env`; pass explicit digests to go anywhere else. Read
   [Roll back](#roll-back) first when the release changed a migration: a
   non-additive migration needs `migrate down` with the image that applied it, or
   a restore. Write down what happened and why, and fix the release before the
   next attempt.
