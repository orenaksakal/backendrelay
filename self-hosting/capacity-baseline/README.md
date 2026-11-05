# Capacity baseline on the production topology (P7.9)

This directory drives the two P7.9 profiles against staging from a desktop, over
the same public path a client uses, and records what the production hardware and
database tier actually hold. Production is one CAX41 (16 shared ARM vCPU, 32 GB)
running the server and PostgreSQL in the same Compose project; staging is the
same Compose file in a second project on that host.

| Profile | Shape | Sessions | Purpose |
| --- | --- | --- | --- |
| `single` | 1 app × 200 users | 200 + 10 observers | One busy app, ten 20-user rooms |
| `many` | 20 apps × 10 users | 200 + 20 observers | Many small tenants, one room each |

Each profile runs steady (300 s), a 2× burst (60 s) and recovery (90 s). A phase
passes only if no operation fails and every observed read, write and realtime
latency stays below 3 s, as in the local benchmark.

Files:

| File | Purpose |
| --- | --- |
| `run.sh` | Runs `single`, `many` or `all` against `CAPACITY_API_ORIGIN`. |
| `../railway-loadtest/remote-scale-run.mjs` | Creates the throwaway apps, seeds them, drives the phases and writes `result.json`. |
| `../railway-loadtest/remote-scale-model.mjs` | The two profile definitions and their phase arithmetic. |

The procedures and the result table live in
[`docs/performance.md#production-topology-staging`](../../docs/performance.md#production-topology-staging).
Read that table before and after the run.

## Before the run

Only ever point this at staging. `run.sh` refuses a loopback origin and an
`api.*` hostname (override the latter with `CAPACITY_ALLOW_PRODUCTION=1` only if
you really mean it).

1. **Staging is deployed and healthy.** `deploy.sh` has run with the digest pair
   under test:

   ```sh
   curl -fsS https://staging-api.<domain>/health/system
   ```

2. **Match the production tier.** Stock staging is deliberately small. Set these
   in `deploy/.env.staging` on the host for the run, copying the production
   values from `deploy/.env`:

   | Setting | Staging default | Production / run value |
   | --- | --- | --- |
   | `INSTANT_SERVER_CPUS` | `1` | `2` |
   | `INSTANT_SERVER_MEM_LIMIT` | `2g` | `8g` |
   | `AUX_POSTGRES_CPUS` | `0.5` | `4` |
   | `AUX_POSTGRES_MEM_LIMIT` | `1g` | `12g` |
   | `CONNECTION_POOL_SIZE` | `10` | `20` |
   | `INSTANT_EPHEMERAL_APPS` | `false` | `true` (throwaway apps) |
   | `INSTANT_MAX_LIVE_SESSIONS_PER_IP` | blank → `100` | `1000` |

   The per-IP limit is the one that silently breaks the run: 210 sessions for
   `single` and 220 for `many` arrive from one desktop address. If a config-app
   `ephemeral-apps-enabled` or `max-live-sessions-per-ip` flag is set, it wins
   over the environment; check with `instant-server flags get <name>` and unset
   it or set the same value.

   `aux-postgres` still starts with `shared_buffers=256MB` and
   `effective_cache_size=1GB` from `deploy/docker-compose.yml`, unlike the
   production `postgres` service (`6GB`/`16GB`). That makes database-side costs
   a conservative lower bound, not an exact production-tier figure. The
   server, CPU, memory and pool settings above do match production.

   Recreate the two services so the limits and flags take effect, then confirm
   them:

   ```sh
   cd /opt/backendrelay/self-hosting
   staging() {
     docker compose --project-name backendrelay-staging --project-directory . \
       --env-file deploy/.env.staging \
       --env-file /var/lib/backendrelay-staging/deploy/current.env \
       --profile staging -f deploy/docker-compose.yml "$@"
   }
   staging up -d --no-deps aux-postgres server
   staging exec -T server /usr/local/bin/instant-server healthcheck
   staging ps --format '{{.Service}} {{.Status}}'
   ```

3. **Choose a quiet window.** Production keeps running on the same host. The
   load is small, but production traffic and backups consume the same CPU and
   disk, so record the window and keep it free of deploys, backups and drills.

4. **Prepare the generator.** Node 20 or newer and `npm` on the desktop. Node 24
   is what the rest of the repository pins (`mise exec node@24 -- bash
   self-hosting/capacity-baseline/run.sh all`). `run.sh` installs the pinned
   generator dependencies on first use and raises the open-file soft limit when
   needed.

## Run

```sh
export CAPACITY_API_ORIGIN=https://staging-api.<domain>
bash self-hosting/capacity-baseline/run.sh all
```

`all` runs `single` then `many`; pass one profile to repeat only that one. Each
profile writes under
`${XDG_STATE_HOME:-~/.local/state}/backendrelay-capacity/baseline/<stamp>/<profile>/`
(`CAPACITY_STATE_DIR` overrides the root). Exit status: `0` passed, `1` a phase
failed, `2` the measurement is invalid (for example the generator saturated or
the server was not healthy). An invalid run is not a capacity result; fix the
cause and rerun.

The run prints each phase window. Keep those timestamps for the CPU capture.

## Capture CPU during the run

Staging deliberately runs no monitoring overlay, so read the container cgroups
on the host while the generator is running. In a second terminal on the host:

```sh
while sleep 1; do
  date -u +%Y-%m-%dT%H:%M:%SZ
  docker stats --no-stream --format \
    '{{.Name}} {{.CPUPerc}} {{.MemUsage}} {{.MemPerc}}' \
    | grep -- 'backendrelay-staging'
done | tee /var/tmp/staging-capacity-stats.log
```

Record the average and peak for `backendrelay-staging-server-1` and
`backendrelay-staging-aux-postgres-1` inside each phase window, as a percentage
of one core (`CPU% / 100` = cores). If you prefer Grafana, the production
Prometheus does not scrape staging containers, so cap a sample loop on the host
the same way.

## Record the results

Fill the table in
[`docs/performance.md#production-topology-staging`](../../docs/performance.md#production-topology-staging):
result, read/write/realtime p95 and max, and server/PostgreSQL cores for each
phase. Keep the `result.json` paths in the notes so the numbers can be traced.

Then decide against the Phase 9 triggers:

- realtime p95 above 1 s with several apps active → head-of-line work (OPS-H3)
- PostgreSQL ms per operation rising from `single` to `many` → OPS-M3
- CPU near the container limits at 200 users → resize before the next wave
- a clean pass with headroom → the launch tier is confirmed

## Clean up

1. Delete the throwaway apps (they are ephemeral and expire after 14 days
   anyway). Their ids are in `result.json` under `appIds`.
2. Restore the staging defaults in `deploy/.env.staging` and recreate (redefine
   the `staging` helper from [Before the run](#before-the-run) if this is a new
   shell):

   ```sh
   staging up -d --no-deps aux-postgres server
   staging exec -T server /usr/local/bin/instant-server flags get ephemeral-apps-enabled
   ```

   `INSTANT_EPHEMERAL_APPS=false` and a blank
   `INSTANT_MAX_LIVE_SESSIONS_PER_IP` restore the committed defaults.
3. Remove the staging throwaway data if you want a clean database for the next
   rehearsal; the apps are disposable by design.
