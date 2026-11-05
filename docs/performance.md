# Performance and capacity

This page describes how BackendRelay's capacity is measured, what the load tests
found, and how to size servers. All numbers come from the load harnesses in this
repository; rerun them on your own hardware before relying on them.

## Workload model

The load generator simulates a mix of typical SaaS applications. Each app follows
one of five templates from
[`dau-model.mjs`](../self-hosting/railway-loadtest/dau-model.mjs), each with its own
read and write cadence per connected user:

| Template | Read every | Write every | Writes to a shared room |
|---|---|---|---|
| Tasks | 30 s | 120 s | 50% |
| Feed | 20 s | 300 s | 70% |
| Analytics | 15 s | 600 s | 10% |
| Chat | 20 s | 30 s | 100% |
| Mobile | 45 s | 180 s | 20% |

- **Users** are concurrently connected sessions, not daily active users.
- **Data:** every user owns 10 private rows, and users are grouped into rooms of 20
  that share messages. A write to a shared room must reach the other 19 members'
  subscriptions in realtime.
- **Reads** are fresh network queries (not cache hits on the client). **Writes**
  wait for the server's acknowledgement.
- **Observers:** one extra session per 20 users subscribes to the writer's data and
  measures realtime delivery latency (write sent → update received).
- At 10,000 users this offers about 445 reads/s and 105 writes/s at steady state.

Each run has three phases: **steady** (300 s), **burst** at 2× the operation
rate (60 s), and **recovery** (90 s). A phase passes only if **no operation
fails** and **every observed read, write and realtime latency stays below 3 s**
(the maximum, not a percentile).

## Results

### Local reference machine

Server and PostgreSQL 17 pinned to 8 physical cores (16 threads) of a fast
desktop x86 CPU; the load generator ran on the remaining cores. Both server and
database ran on the same machine.

| Shape | Users | Result | Steady cores (server + PG) | Burst cores (server + PG) | Worst latency |
|---|---|---|---|---|---|
| 10 apps × 1,000 | 10,000 | ✅ pass (twice) | 3.7 + 2.1 | 7.6 + 5.5 | 1.5 s (burst realtime) |
| 25 apps × 100 | 2,500 | ✅ pass | 1.0 + 0.6 | 1.8 + 1.2 | 66 ms |
| 50 apps × 100 | 5,000 | ✅ pass | 1.9 + 1.5 | 3.8 + 3.0 | 246 ms |
| 100 apps × 100 | 10,000 | ❌ burst failed | 4.6 + 3.9 | 5.8 + 8.9 | steady 710 ms; burst > 9 s |

CPU per operation:

| Shape | Server CPU per op | PostgreSQL CPU per op |
|---|---|---|
| 10 apps × 1,000 | 6.7 ms | 3.8 ms |
| 25 apps × 100 | 7.1 ms | 4.6 ms |
| 50 apps × 100 | 7.1 ms | 5.3 ms |
| 100 apps × 100 | 8.4 ms | 7.1 ms |

Server cost per operation stays flat as the number of apps grows. PostgreSQL cost
grows with the number of small apps, because fewer sessions share each cached
query result and the database sees more distinct statements. See
[Known limits](#known-limits).

### Shared-vCPU ARM server

A 16 vCPU (shared Ampere ARM) / 32 GB virtual server running the server,
PostgreSQL and unrelated containers together, with the generator about 160–290 ms
away over the internet:

| Shape | Users | Result |
|---|---|---|
| 5 apps × 1,000 | 5,000 | Steady: no failures; reads p99 410 ms, realtime max 1.7 s; one write at 3.5 s exceeded the 3 s maximum |
| 5 apps × 2,000 | 10,000 | Steady: reads and writes within limits, realtime p50 3.0 s (CPU-bound) |

On this hardware each operation cost about 2.7× the CPU of the local reference
machine, so a shared-vCPU ARM server of this size tops out just under 5,000
concurrent users of this workload.

### Production topology (staging)

Production is one CAX41 (16 shared ARM vCPU, 32 GB) running the server and
PostgreSQL 17 in the same Compose project. Staging runs the same Compose file in
a second project on that host, so a desktop load generator sees the production
hardware, database build and public network path.

The launch tier is measured with two profiles of 200 concurrent sessions each:
one app carrying all 200 users, and twenty apps carrying ten users each. Both
run steady (300 s), 2× burst (60 s) and recovery (90 s) with the same
observed-maximum 3 s criterion as the local benchmark.

| Profile | Sessions | Result | Read p95 / max | Write p95 / max | Realtime p95 / max | Server cores (steady / burst) | PostgreSQL cores (steady / burst) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 app × 200 users | 200 (+10 observers) | Not measured | — | — | — | — | — |
| 20 apps × 10 users | 200 (+20 observers) | Not measured | — | — | — | — | — |

P7.9 has not been recorded yet. Until it is, the planning numbers above are the
local reference and shared-vCPU results; do not treat the launch tier as
confirmed. The owner runs the baseline from the desktop with staging temporarily
set to the production limits (server 2 CPU / 8 GB, PostgreSQL 4 CPU / 12 GB,
`CONNECTION_POOL_SIZE=20`), then fills this table:

```sh
export CAPACITY_API_ORIGIN=https://staging-api.<domain>
bash self-hosting/capacity-baseline/run.sh all
```

The exact staging preparation, CPU capture and clean-up are in
[`self-hosting/capacity-baseline/README.md`](../self-hosting/capacity-baseline/README.md).
Staging keeps `shared_buffers=256MB` and `effective_cache_size=1GB` while the
production database uses 6 GB and 16 GB, so database-side costs from this run are
a conservative lower bound; the server limits, database build, CPU and host are
the production ones.

These numbers drive the Phase 9 triggers:

- a pass at 200 users with CPU headroom confirms the launch tier
- realtime p95 above 1 s with the twenty-app profile → head-of-line work (OPS-H3)
- PostgreSQL cost per operation higher in the twenty-app profile → OPS-M3
- CPU close to a container limit at 200 users → resize before the next wave

## Sizing

Rules of thumb per **1,000 concurrent users** of the workload above:

| | Server | PostgreSQL |
|---|---|---|
| CPU at steady state (fast x86 cores) | ~0.4 cores | ~0.2–0.3 cores |
| CPU at steady state (shared ARM vCPU) | ~1 core | ~0.8 cores |
| Memory | ~1 GB base + ~0.5 MB per connected session | depends on data size, not users |

Plan for **twice the steady-state CPU** so a 2× burst fits, and keep about 20%
spare on top of that. Prefer dedicated vCPUs once traffic grows: shared vCPUs are
fine at small scale but make burst capacity unpredictable.

Starting points (server and database on separate machines in the same region):

| Stage | Server | PostgreSQL |
|---|---|---|
| Launch, up to ~100 concurrent users | 4 vCPU / 8 GB | 1/4 vCPU / 2 GB, `CONNECTION_POOL_SIZE=16` |
| ~2,000 concurrent users | 4–8 dedicated vCPU / 16 GB | 2 vCPU / 16 GB |
| ~4,000 concurrent users | 8 dedicated vCPU / 32 GB | 4 vCPU / 32 GB |

These estimates were measured with the database on the same machine as the
server. Putting PostgreSQL on a separate host adds a network round trip to every
SQL statement (about 18 per operation, counting the realtime refreshes it
triggers), so keep both in the same region and rerun the load test after moving.
Many small apps cost PostgreSQL more per operation; size the database one step
larger if you host dozens of small tenants. See
[managed-postgres.md](managed-postgres.md) for managed database requirements.

## Running the load tests

### Local benchmark

[`self-hosting/scale-local/`](../self-hosting/scale-local/) runs the whole system
under test on one Linux machine:

```sh
# Provision a disposable PostgreSQL 17.11 and a release-built server, pinned to
# the first 8 physical cores (the generator gets the remaining cores).
SCALE_SUT_PHYSICAL_CORES=8 bash self-hosting/scale-local/stack.sh up

# 10 apps x 1,000 users through steady (300 s), 2x burst (60 s) and recovery (90 s).
bash self-hosting/scale-local/run.sh --apps 10 --users-per-app 1000

# Many small apps.
bash self-hosting/scale-local/run.sh --fresh --apps 50 --users-per-app 100

bash self-hosting/scale-local/stack.sh down
```

- `run.sh` brings the stack up if needed (`--fresh` forces a clean restart) and
  accepts `--apps` (1–100), `--users-per-app` (a multiple of 20, up to 2,000),
  `--steady`, `--burst`, `--recovery`, `--ramp-per-second` and `--out`.
- The generator creates throwaway apps on the local server and never talks to
  anything but the loopback origin recorded by `stack.sh`.
- Results are written under `$XDG_STATE_HOME/instant-scale-local/current/runs/`
  (default `~/.local/state/...`): a `result.json`, one JSON file per phase with
  latencies, CPU per operation and the top PostgreSQL statements, and resource
  samples.
- Exit status: 0 pass, 1 fail, 2 invalid (for example, the generator itself was
  saturated).
- Environment overrides such as `SCALE_SUT_CPUS`, `CONNECTION_POOL_SIZE` and
  `SCALE_SERVER_BINARY` (a prebuilt server) are listed by `stack.sh --help`.

The generator needs a high open-file limit (about 10,500 sockets at 10,000 users).

### Against a remote deployment

[`self-hosting/capacity-baseline/run.sh`](../self-hosting/capacity-baseline/run.sh)
drives the two P7.9 profiles against staging over the public path:

```sh
export CAPACITY_API_ORIGIN=https://staging-api.<domain>
bash self-hosting/capacity-baseline/run.sh all
```

It creates throwaway ephemeral apps, so enable them on staging for the run only.
See [`self-hosting/capacity-baseline/README.md`](../self-hosting/capacity-baseline/README.md)
for the staging limits, CPU capture and clean-up.

The generator modules in
[`self-hosting/railway-loadtest/`](../self-hosting/railway-loadtest/) read the
target origin from `LOADTEST_API_ORIGIN` (default `http://localhost:8888`). Only
point them at deployments you own, and expect results to include the network
round trip between the generator and the server.

## Known limits

- **Realtime fan-out is the first bottleneck.** When CPU runs out, reads and writes
  stay fast but realtime delivery lags, because refreshes for room members queue
  behind each other.
- **Many small apps** raise PostgreSQL cost per operation (about 2× at 100 apps of
  100 users versus 10 apps of 1,000). A permission lookup on `triples` becomes
  several times slower per call in that shape. This is a known optimization
  target.
- **Shared vCPUs** (and slower ARM cores) cost roughly 2–3× more CPU per operation
  than a modern desktop x86 core.
