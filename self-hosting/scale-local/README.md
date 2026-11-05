# scale-local

Runs the whole BackendRelay system under load on one Linux machine: a disposable
PostgreSQL 17.11 and a release-built `instant-server`, both pinned to dedicated
CPUs, plus a load generator on the remaining CPUs. Nothing here touches a real
deployment; the stack only listens on loopback and its generated secrets never
reach disk.

Linux only: it needs `mise`, `taskset`, `lscpu`, and an open-file hard limit of
at least 65,536 (about 10,500 sockets at 10,000 sessions).

## Scripts

| Script | What it does |
| --- | --- |
| `stack.sh up \| down \| status` | Provision/stop the disposable stack. `up` builds `instant-server --release`, migrates, and serves. |
| `run.sh` | Bring the stack up if needed and run one load scenario. Accepts `--apps`, `--users-per-app`, `--steady`, `--burst`, `--recovery`, `--ramp-per-second`, `--out`, `--stack`. |
| `drills.sh` | Inject one failure (`kill9`, `walsender`, `pg-restart`, `invalidate-slot`, `disk-pressure`) into a running stack and time recovery. |
| `weekly.sh` | The scheduled regression: both weekly shapes, kept results, baseline comparison. |
| `weekly_compare.py` | Compare a results directory with `baseline.json` (`--update-baseline` rewrites it). |

```sh
bash self-hosting/scale-local/stack.sh up
bash self-hosting/scale-local/run.sh --apps 10 --users-per-app 1000
bash self-hosting/scale-local/stack.sh down
```

State and logs default to `$XDG_STATE_HOME/instant-scale-local/current` (override
`SCALE_STATE_DIR`); results land in that directory's `runs/`. Exit status of a run
is 0 pass, 1 fail, 2 invalid (for example, the generator itself was saturated).
See [performance.md](../../docs/performance.md) for the methodology and measured
numbers, and [operations.md](../../docs/operations.md#failure-drills) for the
failure drills.

## Weekly regression

`weekly.sh` provisions its own stack (state in
`~/.local/state/instant-scale-local/weekly-state`, so an interactive stack in
`current` is never touched), runs both weekly shapes — 10 apps × 1,000 users and
50 apps × 100 users — copies each run's `result.json` to
`~/.local/state/instant-scale-local/weekly/<UTC date>/<scenario>.json`, stops the
stack, and compares the results with [`baseline.json`](baseline.json). Override
the results root with `WEEKLY_OUT`, the stack state with `WEEKLY_STATE_DIR`, and
the shortened phase lengths with `WEEKLY_STEADY`, `WEEKLY_BURST` and
`WEEKLY_RECOVERY` (120/30/30 seconds, versus the 300/60/90 defaults).

The comparison fails (exit 1) when a run reports a client-visible error, or when
the worst-phase p95 operation latency, server CPU-ms per operation, or PostgreSQL
CPU-ms per operation is more than 20% above the baseline. Metrics whose baseline
value is still `null` are reported as unmeasured rather than compared. After a
deliberate change in cost — or the first run — rewrite the baseline from a
results directory and commit it:

```sh
python3 self-hosting/scale-local/weekly_compare.py \
  --results-dir ~/.local/state/instant-scale-local/weekly/2026-11-01 \
  --update-baseline
```

Run the weekly check before a release that touches the realtime or query path:

```sh
bash self-hosting/scale-local/weekly.sh
```

### Desktop timer

`systemd/` holds the units installed as **user** units on the desktop that runs
the benchmark. They are not installed by anything in this repository:

```sh
chmod +x self-hosting/scale-local/weekly.sh   # the unit runs it directly
mkdir -p ~/.config/systemd/user
cp self-hosting/scale-local/systemd/backendrelay-weekly-scale.service \
   self-hosting/scale-local/systemd/backendrelay-weekly-scale.timer \
   ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now backendrelay-weekly-scale.timer
systemctl --user list-timers backendrelay-weekly-scale.timer
```

The timer runs Sundays at 04:00 local time and catches up after a missed window
(`Persistent=true`); the service is a `oneshot` with `Nice=10`. Its `ExecStart`
points at `%h/Projects/backendrelay`, so adjust it if the checkout lives
elsewhere. Inspect the last run with:

```sh
systemctl --user status backendrelay-weekly-scale.service
journalctl --user -u backendrelay-weekly-scale.service -n 100
```
