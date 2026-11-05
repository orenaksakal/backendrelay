# Abuse and isolation drill (P7.10)

This kit hammers one throwaway app on staging and proves that a second app, and
the deployment around it, do not feel it. It covers the public limits added by
P2.3 (magic-code quota), P2.8 (request body, InstaQL size and live sessions per
IP), P2.10 (upload size and content headers) and P3.5 (per-app fairness). The
procedures and the result table live in
[`docs/operations.md#abuse-and-isolation-drills`](../../../docs/operations.md#abuse-and-isolation-drills).

It only ever creates apps; it never touches an existing app. Run it against
staging, from a desktop, over the same public path a browser uses: the per-IP
limits are resolved through `INSTANT_TRUSTED_PROXY_CIDRS`, so a direct
container-network call sees a private address and skips them.

This drill covers fairness and limits. Credential isolation (TST-H3) is the
matching end-to-end suite,
`bash self-hosting/run-cross-app-isolation-acceptance.sh`: A's credentials must
never reach B, while this drill proves A's traffic cannot hurt B.

| File | Purpose |
| --- | --- |
| `abuse-drill.mjs` | Creates apps A and B, measures B, hammers A, writes `result.json`. |
| `abuse-model.mjs` | Percentile and verdict arithmetic shared with the test. |
| `abuse-model.test.mjs` | `node --test` coverage for the verdicts; needs no server. |

## Before the run

1. **Staging is deployed and healthy.** Use the P7.5 staging project on the
   production host, with its own database and buckets:

   ```sh
   curl -fsS https://staging-api.<domain>/health/system
   ```

2. **Match the production tier.** The p95 comparison is only meaningful when
   staging has the production CPU, memory and pool sizes (the table in
   [`../../capacity-baseline/README.md`](../../capacity-baseline/README.md#before-the-run)).
   Set them in `deploy/.env.staging`, recreate `aux-postgres` and `server`, and
   keep the window free of deploys, backups and other staging traffic.

3. **Set the limits the drill will hit.** Staging defaults are mostly already
   correct; make them explicit for the window:

   | Setting | Value for the drill | Why |
   | --- | --- | --- |
   | `INSTANT_EPHEMERAL_APPS` | `true` | The drill creates its two apps. |
   | `INSTANT_MAX_LIVE_SESSIONS_PER_IP` | `100` | The session check tries 101. |
   | `INSTANT_MAGIC_CODE_RATE_LIMIT_PER_IP_HOUR` | `30` | The send check stops at the first refusal after 30. |
   | `INSTANT_MAGIC_CODE_DAILY_LIMIT_PER_APP` | `1000` | Not reached from one IP; see the operator checks. |
   | `INSTANT_STORAGE_MAX_UPLOAD_BYTES` | `1048576` | Keeps the oversize upload at 1 MiB + 1. |
   | `INSTANT_TRUSTED_PROXY_CIDRS` | the staging Docker subnet | Otherwise the per-IP checks are disabled. |

   Config-app flags override the environment. Read and unset the ones that
   collide, or align the `ABUSE_EXPECTED_*` values with them:

   ```sh
   staging exec -T server /usr/local/bin/instant-server flags get max-live-sessions-per-ip
   staging exec -T server /usr/local/bin/instant-server flags get magic-code-rate-limit-per-ip-hour
   staging exec -T server /usr/local/bin/instant-server flags get storage-quota-bytes
   ```

   `storage-quota-bytes` must be unset or larger than the small HTML upload;
   `INSTANT_STORAGE_MAX_UPLOAD_BYTES` restores the committed default after the
   run. Leave `max-sessions-per-app` at its default: opening 10,000 sessions
   from one desktop would test the desktop, not the server.

4. **Keep the desktop's other sessions closed.** The magic-code buckets are
   scoped to the app, so the new app starts fresh; the live-session cap is
   process-wide and counts every session from the desktop's public address,
   including a browser tab on staging. Close them for the window.

5. **Prepare the generator.** Node 24, as the rest of the repository pins. The
   driver uses the global `WebSocket` (Node 22 or newer) for the query-size
   check:

   ```sh
   mise exec node@24 -- node --version
   ```

## Run

```sh
export ABUSE_API_ORIGIN=https://staging-api.<domain>
mise exec node@24 -- node self-hosting/drills/abuse/abuse-drill.mjs
```

The driver refuses a loopback origin (`ABUSE_ALLOW_LOOPBACK=true` overrides it
for a local smoke run) and an `api.*` hostname (`ABUSE_ALLOW_PRODUCTION=true`
overrides that, which is not what this drill wants).

It prints the two app IDs, the baseline and attacked p95, one line per check,
the attack window and the path to `result.json`
(`${XDG_STATE_HOME:-~/.local/state}/backendrelay-abuse/<stamp>/result.json`).
Exit status: `0` every check passed, `1` a check failed, `2` the run is invalid
(setup failed, a request never completed, or the origin was refused). The JSON
holds only app IDs, timings, status codes and verdicts: no tokens, emails or
signed URLs.

Default shape: 30 s baseline, 60 s attack, four concurrent app B readers, eight
concurrent expensive queries on A, 200 guest sign-ups, 2,000 seeded rows. Every
value has an `ABUSE_*` override; see the constants at the top of the driver.
Shorten `ABUSE_BASELINE_SECONDS` and `ABUSE_HAMMER_SECONDS` only for a smoke run,
and record that you did.

Both the baseline and the expensive-query load use `/admin/query`, so they
measure the same interactive PostgreSQL pool that P3.5 partitions; the admin
token bypasses app rules so the cost stays in the database rather than in rule
evaluation.

## What the checks mean

| Check | Passes when |
| --- | --- |
| Pre-auth body | 1 MiB + 1 byte to `/dash/apps` returns 413 before authentication. |
| InstaQL query size | a WebSocket `add-query` above 256 KiB returns `parameter-limit-exceeded`. |
| Magic-code sends to one address | exactly 20 accepted, then 429 `rate-limited` (the address is fresh). |
| Magic-code sends from one IP | exactly 30 accepted, then 429 `rate-limited`. |
| Oversize upload | `INSTANT_STORAGE_MAX_UPLOAD_BYTES + 1` returns 413. |
| HTML upload | upload is 2xx, the presigned download says `attachment`, and the type is not `text/html`. |
| Guest sign-up burst | at least one 2xx and no 5xx. There is no dedicated guest-sign-up quota, so this is a fairness check, not a limit. |
| Expensive queries | at least one 2xx and no request above `ABUSE_QUERY_CEILING_MS` (30 s default). |
| Sessions from one IP | at most 100 upgraded, then a 429 `rate-limited` refusal. |
| App B p95 | within 1.5x its own baseline or 250 ms, whichever is larger. |
| App B read errors | no non-2xx admin reads. |

Rerunning against the same app would reuse its rate-limit buckets; the driver
always creates new apps, so the send buckets start full. The batch is
all-or-none, so a refusal consumes nothing: a fresh app should reach exactly 30.
A lower count means the configured cap is lower or another client used the
bucket. The live-session cap is process-wide, so close other sessions from the
same address for the full count. The per-address bucket is always fresh because
the drill uses a new address, so its count must match exactly.

## Operator evidence the script cannot collect

The script records the attack window (`startedAt` / `endedAt`) so these can be
read for the same minutes.

- **Alerts.** Staging normally runs no monitoring overlay, so the production
  Prometheus does not scrape it. If a Prometheus that scrapes staging is
  attached, confirm that the alerts for the saturated surface fired and record
  their names. Otherwise this is an owner decision, not a silent pass; note it
  in the result table. Metrics are off unless `INSTANT_METRICS_ADDR` is set;
  the monitoring overlay uses `0.0.0.0:9091` with
  `INSTANT_METRICS_ALLOW_NONLOOPBACK=true`, and a staging scraper needs the same
  private binding. Useful queries, with the `prom` helper from
  [`docs/operations.md#finding-problems`](../../../docs/operations.md#finding-problems):

  ```sh
  prom 'ALERTS{alertstate="firing"}'
  prom 'sum by (alertname) (ALERTS{alertstate="firing"})'
  ```

  The surfaces this drill can saturate map to `AdmissionPressure`,
  `RequestLatency`, `RealtimeRefreshBacklog`, `RealtimeRefreshRejected`,
  `PoolPressure` and `WebsocketExitsHigh`.

- **Email provider.** The quota must hold at the provider, not only at the
  server. In the provider console, count sends for the window; it must not
  exceed the accepted magic-code sends (30 when fresh) and must never pass the
  daily quota. Cross-check `backendrelay_emails_total{outcome="sent"}` against
  `{outcome="rate_limited"}` when a scraper is attached: the rate-limited
  count is the refused requests, and those must not appear as provider sends.

- **Daily quota, storage quota and subscription cap.** None is reached from one
  desktop in one run. Confirm that `magic-code-daily-limit-per-app`,
  `storage-quota-bytes` and `max-subscriptions-per-session` are set to the
  intended values and rely on their live tests; or run the magic-code daily
  quota check across several public egress addresses.

- **Database timeouts.** `statement_timeout`, `lock_timeout` and
  `idle_in_transaction_session_timeout` are set per pool connection, so
  `SHOW` on a fresh `psql` does not prove them. The expensive-query ceiling and
  app B's p95 are the observable side; P3.5's live tests cover the rest.

## Clean up

1. Delete apps A and B from the dashboard. They are ephemeral and expire after
   14 days even if left alone, but deleting them removes their rows and files.
2. Restore the staging defaults (`INSTANT_STORAGE_MAX_UPLOAD_BYTES`,
   `INSTANT_MAX_LIVE_SESSIONS_PER_IP`, `INSTANT_EPHEMERAL_APPS`) and recreate
   `server` if you changed them.
3. Copy the measured row into
   [`docs/operations.md#abuse-and-isolation-drills`](../../../docs/operations.md#abuse-and-isolation-drills)
   and keep `result.json` with the drill notes.
