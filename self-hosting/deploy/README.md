# Deployment kit

This directory is the production path chosen in D3: one host, Docker Compose,
Caddy, a self-managed PostgreSQL 17, and signed server and dashboard images.
It replaces the manual upgrade procedure in
[`docs/operations.md#deploys`](../../docs/operations.md#deploys) with a
repeatable, health-gated deploy and an automatic rollback.

| File | Purpose |
| --- | --- |
| `docker-compose.yml` | Production services: `postgres`, `server`, `dashboard`, `caddy`, and the staging-only `aux-postgres`. The `smoke` service is profile-gated. The same file runs the staging project. |
| `Caddyfile` | Cloudflare origin-certificate edge with the Cloudflare `trusted_proxies`, request limits, SSE flushing and `/health/system` health checks. |
| `Caddyfile.letsencrypt-dns` | Alternative edge for Let's Encrypt DNS-01. Needs a Caddy image built with `caddy-dns/cloudflare`. |
| `deploy.sh` | Verify, back up, migrate, restart one server replica at a time, restart the dashboard and smoke-test. Rolls back on failure. |
| `rollback.sh` | Return to the last good images, rolling migrations down when the P3.9 additive floor does not cover them. |
| `.env.example` | Every setting the kit reads, with comments. Copy it to `.env` (mode 0600). |
| `.env.staging.example` | The staging project's settings: own project, network, ports, database, buckets and state directory. Copy it to `.env.staging` (mode 0600). |
| `staging-backup.sh` | Pre-deploy `pg_dumpall` of the staging database, run through `DEPLOY_BACKUP_COMMAND`. |
| `smoke.mjs` | Dependency-free SDK-protocol query used by the deploy smoke test. |
| `systemd/` | Backup, canary and weekly audit units. |
| `docker-compose.dry-run.yml`, `Caddyfile.local` | Rehearse the stack on a desktop with MinIO and the local `postgres` service. |
| `tls/` | Mount point for the origin certificate; keys are git-ignored. |

The kit assumes the Caddy-based dashboard image from DASHBOARD.md D1.4/D9.4
(port 8080, `BR_API_URL`), not `client/Dockerfile.www`.

## Prerequisites

- A host provisioned as described in [`../provision/README.md`](../provision/README.md):
  Ubuntu 24.04, key-only SSH, and inbound 80/443 restricted to Cloudflare by the
  Hetzner Cloud Firewall.
- Docker Engine with Compose v2, `cosign`, `curl`, `python3` and `age` on the
  host.
- A Cloudflare origin certificate for `BACKEND_DOMAIN` and `DASHBOARD_DOMAIN`
  in `DEPLOY_TLS_DIR`, with Cloudflare SSL mode **Full (strict)** and DNS
  proxied.
- An age key pair for the secrets backup. The public recipient goes in the
  backup command; the identity stays offline. See
  [`docs/operations.md#secrets`](../../docs/operations.md#secrets).
- A `recovery.py` configuration at `DEPLOY_RECOVERY_CONFIG` with a tested
  `uploadCommand` and `verifyOffhostCommand`. The deploy aborts without a
  successful backup.
- Signed server and dashboard images. `verify-release-images.sh` requires a
  `COSIGN_PUBLIC_KEY` or a certificate identity/issuer; see
  [`docs/self-hosting.md`](../../docs/self-hosting.md#building-images).
- A canary app and a root-owned admin-token file (P7.6) before the first
  non-bootstrap deploy.

## First deploy

```sh
cd /opt/backendrelay/self-hosting
cp deploy/.env.example deploy/.env
chmod 600 deploy/.env
# Fill in digests, domains, TLS directory, database, storage and email values.
# Put origin.pem and origin-key.pem in DEPLOY_TLS_DIR.

# Generate the server keysets once, put them in the server_secrets volume and
# store a copy in the password manager. Do not let the server auto-generate a
# second file on another instance. See docs/operations.md#secrets.

DEPLOY_SKIP_CANARY_SMOKE=1 DEPLOY_FIRST_DEPLOY=1 bash deploy/deploy.sh \
  registry.example.com/backendrelay/server@sha256:<digest> \
  registry.example.com/backendrelay/dashboard@sha256:<digest>
```

`DEPLOY_FIRST_DEPLOY=1` is only for an empty database where there is no state to
back up. Every later deploy must use the verified `recovery.py` backup. If the
stack is already serving before this first kit deploy, omit the flag and make
the recovery config's `candidateImages` match the running digests; `deploy.sh`
rewrites that field in a temporary copy for the pre-deploy backup anyway.

`DEPLOY_SKIP_CANARY_SMOKE=1` is only for the bootstrap, before the canary app
exists. Create the app and token, set `CANARY_APP_ID` and
`CANARY_ADMIN_TOKEN_FILE`, then run the same command without the skip.

After this bootstrap, finish the go-live steps in
[First production deploy (P7.6)](../../docs/operations.md#first-production-deploy-p76):
restrict the dashboard before it is public, pin `ephemeral-apps-enabled` and
`app-metric-slugs` with `flags set`, create the production canary app, and
confirm a clean `instant-server audit`.

The script refuses a `.env` that is readable or writable by group or other,
refuses non-digest image references, and refuses to run two deploys at once.

## Routine deploy

```sh
bash deploy/deploy.sh <server-digest-image> <dashboard-digest-image>
```

Rehearse the same digests on staging first; see
[Staging on the same host](#staging-on-the-same-host-p75).

The order is deliberate:

1. Verify both image signatures.
2. Start `postgres` if it is stopped, take a pre-deploy `recovery.py backup`,
   and abort if it fails. Set `DEPLOY_BACKUP_COMMAND` for a managed provider's
   on-demand backup instead.
3. `migrate up` with the new server image.
4. Recreate one server replica at a time and wait up to
   `DEPLOY_HEALTH_TIMEOUT_SECONDS` (60) for its own `healthcheck`.
5. Recreate the dashboard and wait for `/healthz`.
6. Recreate Caddy only when its config or image changed.
7. Smoke-test the public `/health/system`, the client-IP chain, one SDK query
   against the canary app, and `instant-server audit`.

Any failure after the first mutation invokes `rollback.sh` with the images from
`$DEPLOY_STATE_DIR/current.env` (default `/var/lib/backendrelay/deploy`). The
backup has already run, so a failed migration leaves a recovery point.

`instant-server audit` exits non-zero on findings, so a deploy fails when an app
has no rules, a `$default` allow-all rule, no owner, or the config app is
unlocked. Fix the finding rather than skipping the step.

### Image state

`$DEPLOY_STATE_DIR` holds `previous.env`, `candidate.env` and `current.env`.
`current.env` is only written after a successful smoke test. Manual Compose
commands should include it after `.env` so they use the deployed digests:

```sh
docker compose --project-directory . --env-file deploy/.env \
  --env-file /var/lib/backendrelay/deploy/current.env \
  -f deploy/docker-compose.yml ps
```

## Rollback

```sh
bash deploy/rollback.sh                            # previous.env
bash deploy/rollback.sh <server> <dashboard>       # explicit images
```

The script first tries `migrate up` with the target image. If the target binary
refuses a newer non-additive schema, it reads the database and binary versions
from the error, runs `migrate down <steps>` with the image that applied them,
and retries. `ROLLBACK_MIGRATE_DOWN=never` turns that into a hard stop when the
down migration would discard data you need; restore the pre-deploy backup
instead. Automatic rollback from `deploy.sh` uses `auto`. Without a state
directory, pass `DEPLOY_ROLLBACK_CURRENT_SERVER=<image>` so the script knows
which image applied the schema.

## One server replica, and adding a second

Start with one `server` container. `deploy.sh` loops over
`DEPLOY_SERVER_SERVICES`, recreating and health-checking each replica before it
touches the next, and Caddy health-checks every upstream. To add a second
replica later:

1. Copy the `server` service in a local override, rename it `server-b`, give it
   `SERVER_B_IP=172.30.0.7` and its own memory/CPU limits.
2. Set `SERVER_UPSTREAMS=server:8888 server-b:8888`.
3. Set `DEPLOY_SERVER_SERVICES="server server-b"`.
4. Raise PostgreSQL `max_connections` or lower `CONNECTION_POOL_SIZE` for the
   extra pool before the first two-replica deploy.

Do not add a replica until the connection budget is checked. The sockets and
SSE sessions still reconnect when one replica restarts; the second replica only
shortens that window.

## Client IPs and the edge

The compose network has a fixed subnet (`DEPLOY_SUBNET`) and fixed service
addresses. Caddy trusts only Cloudflare's published ranges, reads
`CF-Connecting-IP`, and replaces `X-Forwarded-For` with `{client_ip}`. The
server's `INSTANT_TRUSTED_PROXY_CIDRS` is set to the same subnet, so per-IP
limits and `request.ip` rules see the public client address.

The deploy smoke test reads Caddy's JSON access log for a marker request and
fails when the resolved address is private, when it equals the Cloudflare edge
address, or when the server logged the "per-IP magic-code quota is inactive"
warning. `DEPLOY_EXPECT_CLOUDFLARE=0` is for a direct edge without Cloudflare;
`local` skips the public-address assertions in the Docker dry run.

The edge also enforces the P2.8 controls: a 10 s header read timeout, a 2 minute
idle timeout, no body timeout, a 2 MB management body limit around the server's
1 MiB cap, 110 MB storage uploads and a 1100 MB dashboard restore upload.

## TLS

- **Origin certificate (default).** `Caddyfile` loads
  `/etc/caddy/tls/origin.pem` and `/etc/caddy/tls/origin-key.pem`. Cloudflare
  terminates public TLS; the host firewall (P7.3) restricts the origin to
  Cloudflare's ranges.
- **Let's Encrypt DNS-01.** Build Caddy with
  `xcaddy build --with github.com/caddy-dns/cloudflare`, set
  `CADDY_CONFIG_FILE=./deploy/Caddyfile.letsencrypt-dns`, and set
  `CLOUDFLARE_DNS_API_TOKEN` (Zone:DNS:Edit). Keep the Cloudflare proxy on if
  you want the same client-IP behaviour.

## Staging on the same host (P7.5)

Staging is the same compose file in a second compose project on the production
host. Every release is rehearsed there with the same `deploy.sh`, so production
only sees images, migrations and edge settings that already passed once.

| | Production | Staging |
| --- | --- | --- |
| `DEPLOY_STACK` | `production` | `staging` |
| Env file | `deploy/.env` | `deploy/.env.staging` |
| Compose project | `backendrelay` | `backendrelay-staging` |
| Database service | `postgres` | `aux-postgres` (`aux-db` volume) |
| Network / subnet | `backendrelay` / `172.30.0.0/24` | `backendrelay-staging` / `172.31.0.0/24` |
| Host ports | 80 / 443 | 8080 / 8443 |
| Rollback state | `/var/lib/backendrelay/deploy` | `/var/lib/backendrelay-staging/deploy` |
| Object storage | `instantdb-files`, `instantdb-backups` | `instantdb-staging-files`, `instantdb-staging-backups` |
| Public hostnames | `api.`, `dash.` | `staging-api.`, `staging-dash.` |

`DEPLOY_STACK` is the only switch. `deploy.sh` and `rollback.sh` derive the
database service, the `--profile staging` flag and the exported
`DEPLOY_DB_SERVICE` from it, and refuse a staging file that reuses the
production project name, network name or state directory. The production
project never starts `aux-postgres` (it is profile-gated) and the staging
project never starts `postgres`: `deploy.sh` starts the stack's own database
first and recreates the other services with `--no-deps`.

Volumes are prefixed with the compose project name, so the staging `server_secrets`
volume and database are separate objects even though the services share names.

Run the staging stack through `deploy.sh` (or explicit `docker compose ... up -d
<service>` commands). A bare `up -d` in the staging project would also start the
unused `postgres` service; it is harmless but wasteful, which is why
`.env.staging.example` gives it small limits.

### One-time staging setup

1. **Buckets and token.** Create `instantdb-staging-files` and
   `instantdb-staging-backups` in R2 with a bucket-scoped token, and add a CORS
   rule for `https://staging-dash.<domain>`. The server has no key-prefix
   setting, so staging uses separate buckets rather than a prefix.
2. **DNS, certificate and ports.** Point `staging-api.<domain>` and
   `staging-dash.<domain>` at the host, proxied. Put an origin certificate that
   covers both in `DEPLOY_TLS_DIR` (`/etc/backendrelay/staging-tls` in
   `.env.staging.example`). Production owns 80/443, so staging binds 8080/8443;
   a Cloudflare **Origin Rule** must send HTTPS for the two staging names to
   origin port **8443**. Allow 8443 from Cloudflare's ranges in the Hetzner
   Cloud Firewall and in `ufw` (see
   [`../provision/README.md`](../provision/README.md#2-hetzner-cloud-firewall)).
3. **Environment file.**

   ```sh
   cd /opt/backendrelay/self-hosting
   cp deploy/.env.staging.example deploy/.env.staging
   chmod 600 deploy/.env.staging
   ```

   Fill in the same digest-only image references as production (the point is to
   run the exact images that go live next), the staging domains, the staging
   database passwords and the staging buckets. `POSTGRES_DB`/`INSTANT_DB_*` must
   match `AUX_POSTGRES_DB`/`AUX_DB_*`: the server connects to `aux-postgres` with
   the first pair, and `aux-postgres` creates that role and database from the
   second.
4. **Staging keysets.** Run `instant-server generate-secrets` once against the
   staging volume (`backendrelay-staging_server_secrets`) with the procedure in
   [`docs/operations.md#secrets`](../../docs/operations.md#secrets), and store
   the file in the password manager separately. Never reuse production keys and
   never let the container generate them on a second instance.
5. **First rehearsal** against the empty `aux-db`. It still takes a (trivial)
   dump, so the staging run follows the production order exactly:

   ```sh
   DEPLOY_ENV_FILE=deploy/.env.staging DEPLOY_SKIP_CANARY_SMOKE=1 \
     bash deploy/deploy.sh <server> <dashboard>
   ```

6. **Staging canary.** Create an app in the staging dashboard, write its admin
   token to `/etc/backendrelay/staging-canary-admin-token` (0600), set
   `CANARY_APP_ID` and `CANARY_ADMIN_TOKEN_FILE`, then drop
   `DEPLOY_SKIP_CANARY_SMOKE`.

Monitoring stays off (`DEPLOY_MONITORING=0`): its Grafana port, exporters and
log shipper belong to production.

### Rehearsing a release

```sh
DEPLOY_ENV_FILE=deploy/.env.staging bash deploy/deploy.sh <server> <dashboard>
bash deploy/deploy.sh <server> <dashboard>   # production, same digests
```

The staging run takes a local dump, migrates, restarts one replica at a time,
recreates the dashboard and Caddy, and runs the same public smoke tests,
canary query and `instant-server audit` as production. Only when it is green
does the same pair of digests go to production. Rehearse an automatic rollback
now and then by deploying an image that fails its health check and confirming
`rollback.sh` restores the previous digests.

Staging is also the restore target for the disaster-recovery drill in P7.8(a):
restore the latest production backup into the staging project (that replaces the
staging database and `server_secrets`) and verify apps, files and auth.

### Staging backups

`staging-backup.sh` runs inside the pre-deploy step through
`DEPLOY_BACKUP_COMMAND` and dumps `aux-postgres` with `pg_dumpall`. It keeps the
newest five dumps under `STAGING_BACKUP_DIR`, never touches the production
database, and refuses to run unless `DEPLOY_STACK=staging`. Restore one with:

```sh
gunzip -c /var/lib/backendrelay-staging/backups/staging-<stamp>.sql.gz |
  docker compose --project-name backendrelay-staging --project-directory . \
    --env-file deploy/.env.staging --profile staging \
    -f deploy/docker-compose.yml exec -T aux-postgres psql -U postgres
```

Staging data is disposable, so this dump is a rehearsal convenience and not an
off-host backup. The staging `server_secrets` volume is not part of it.

## Monitoring overlay

The monitoring stack is opt-in because it needs its own digest-pinned images
and secret files:

```sh
DEPLOY_MONITORING=1 bash deploy/deploy.sh <server> <dashboard>
```

or manually:

```sh
docker compose --project-directory . --env-file deploy/.env \
  -f deploy/docker-compose.yml -f docker-compose.monitoring.yml up -d
```

The canary profile is started by `systemd/backendrelay-deploy-canary.service`.

Once the overlay is up, run the read-only `monitoring/live-check.sh` and the
alert and dead-man tests in
[Monitoring go-live and alert tests](../../docs/operations.md#monitoring-go-live-and-alert-tests-p77)
before treating monitoring as live.

## systemd units

```sh
sudo install -m 0644 deploy/systemd/backendrelay-deploy-*.service \
  deploy/systemd/backendrelay-deploy-*.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now backendrelay-deploy-backup.timer
sudo systemctl enable --now backendrelay-deploy-audit.timer
sudo systemctl enable --now backendrelay-deploy-canary.service
```

The units assume the repository is at `/opt/backendrelay`; adjust the paths if
it is elsewhere. Do not install the duplicate backup units in
`self-hosting/monitoring/` alongside these, or the backup runs twice. Check a
unit with `systemctl cat` and run it once by hand before enabling the timer.

## Local dry run

This rehearses the whole stack on a desktop with MinIO and the bundled
`postgres` service. It needs local images and no Cloudflare account.

1. Build the images locally: `docker build -t backendrelay/server:dry-run
   server-rs` and the D1.4 dashboard image. Build or pull the PostgreSQL image
   from `self-hosting/postgres`.
2. Copy `.env.example` to `.env` and set:
   - `BACKEND_DOMAIN=api.localhost`, `DASHBOARD_DOMAIN=dash.localhost`
   - `INSTANT_BACKEND_URL=http://api.localhost`,
     `INSTANT_DASHBOARD_URL=http://dash.localhost`
   - `INSTANT_SECURE_COOKIES=false`
   - `CADDY_CONFIG_FILE=./deploy/Caddyfile.local`
   - `DEPLOY_TLS_DIR=./deploy/tls`
   - `S3_ENDPOINT=http://minio:9000`,
     `S3_PUBLIC_ENDPOINT=http://localhost:9000`
   - `DEPLOY_EXPECT_CLOUDFLARE=local`,
     `DEPLOY_SMOKE_API_URL=http://api.localhost`
   - `DEPLOY_SKIP_CANARY_SMOKE=1` until a local canary app exists.
3. Start the stack and apply migrations:

   ```sh
   docker compose --project-directory . --env-file deploy/.env \
     -f deploy/docker-compose.yml -f deploy/docker-compose.dry-run.yml up -d
   docker compose --project-directory . --env-file deploy/.env \
     -f deploy/docker-compose.yml -f deploy/docker-compose.dry-run.yml \
     run --rm server migrate up
   ```

4. Check `curl -s http://api.localhost/health/system` and the dashboard on
   `http://dash.localhost`.
5. Exercise `deploy.sh` with signed images (the script rejects local unsigned
   tags by design) or run the same steps manually. The signature check is the
   only step that cannot be reproduced with locally built images.

A full rehearsal must include one deliberately failed deploy (for example a
server image that fails its health check) and confirm that `rollback.sh`
restores the previous digests and `/health/system`.

## Owner actions

- Fill `deploy/.env` and the TLS directory on the production host.
- Restrict the dashboard before the first public deploy: allowed emails, closed
  signup, a superuser address, and an optional gateway token only after the
  gateway injects the header.
- Pin the initial config flags with `flags set` (`ephemeral-apps-enabled` false
  and `app-metric-slugs`), create the production canary app and its token file,
  and confirm `instant-server audit` is clean. See
  [First production deploy (P7.6)](../../docs/operations.md#first-production-deploy-p76).
- Create the age key pair, generate `secrets.env` once into the `server_secrets`
  volume, and store copies in the password manager. Run
  [`../backup/secrets-backup.sh`](../backup/secrets-backup.sh) once with an R2
  upload and once to offline media.
- Create the registry pull credentials and the Pushover/Grafana/dead-man secret
  files.
- Stand up the staging project: the two staging buckets and token, the
  `staging-api.`/`staging-dash.` records with an Origin Rule to origin port
  8443, the 8443 firewall rule, a filled `deploy/.env.staging` and its own
  keysets. Rehearse one release and one deliberate rollback there.
- Run the full Docker rehearsal above and one deliberate rollback before the
  first production deploy. Verify the off-host secrets copy monthly
  ([`docs/operations.md#secrets`](../../docs/operations.md#secrets)).
