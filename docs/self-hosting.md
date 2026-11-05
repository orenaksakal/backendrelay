# Self-hosting BackendRelay

This guide covers running BackendRelay in production. To try it locally first, see [getting-started.md](getting-started.md). [configuration.md](configuration.md) lists every environment variable.

## Components

| Component | Source | Port | Role |
| --- | --- | --- | --- |
| `server` | `server-rs/Dockerfile` (`instant-server` binary) | 8888 | HTTP API, WebSocket (`/runtime/session`) and SSE (`/runtime/sse`) runtime, Admin SDK, and dashboard API |
| `migrate` | Same image, run as `migrate up` | – | One-shot schema migration. The server verifies the schema but never migrates it. |
| `www` (dashboard) | `client/Dockerfile.www` | 3000 | Dashboard at `/dash`, plus the bundled site and docs |
| `postgres` | `self-hosting/postgres/` (PostgreSQL 17 with wal2json, pg_hint_plan, and pgBackRest) | 5432 | All application data. Realtime runs on logical decoding. |
| Object storage | External, S3-compatible (Cloudflare R2 in the shipped files) | – | User files bucket and per-app backups bucket |
| Proxy | Caddy (`docker-compose.with-caddy.yml`), or your own reverse proxy | 80/443 | TLS termination and routing |
| Monitoring | `docker-compose.monitoring.yml` | – | Prometheus, Alertmanager, Grafana, exporters, and optional Bugsink error tracking |

## Deployment options

All files are in [`self-hosting/`](../self-hosting/). Every production-oriented file except `docker-compose.test.yml` requires Cloudflare R2 credentials, because it builds the S3 endpoint as `https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com`. To use another provider, see [Object storage](#object-storage).

| File | Runs | Published ports | Migrations | Use when |
| --- | --- | --- | --- | --- |
| [`docker-compose.with-caddy.yml`](../self-hosting/docker-compose.with-caddy.yml) + [`Caddyfile`](../self-hosting/Caddyfile) | postgres, server, www, caddy | 80, 443 | Run manually | One host with automatic HTTPS. This is the simplest production path. |
| [`docker-compose.yml`](../self-hosting/docker-compose.yml) | postgres, server, www | `127.0.0.1:8890` (PostgreSQL only) | Run manually | Base file to extend with your own proxy or port mappings |
| [`docker-compose.local.yml`](../self-hosting/docker-compose.local.yml) | postgres, server, www (both built locally) | 8888, 3000, 8890 on `127.0.0.1` | Run manually | Local testing against real R2 |
| [`docker-compose.test.yml`](../self-hosting/docker-compose.test.yml) | postgres, RustFS, server, www | Loopback only | Run manually | Local development and CI. Not for production. |
| [`docker-compose.monitoring.yml`](../self-hosting/docker-compose.monitoring.yml) | Overlay | Grafana on `127.0.0.1:3001`; Bugsink on `127.0.0.1:8000` (`errors` profile) | – | Adds monitoring to the base compose file. See [Monitoring](#monitoring-and-health) and [Error tracking](operations.md#error-tracking). |
| [`deploy/`](../self-hosting/deploy/) | postgres, server, dashboard, caddy, aux-postgres (a second `staging` project uses the same file) | 80, 443 (staging: 8080, 8443) | `deploy.sh` | Production kit with signed digests, a verified pre-deploy backup, health-gated restarts, automatic rollback, a rehearsed `staging` project on the same host and systemd backup/audit/canary units. See [`deploy/README.md`](../self-hosting/deploy/README.md). |

Notes that apply across files:

- **Image defaults.** When `INSTANT_DASHBOARD_IMAGE` is unset, `docker-compose.yml` and `docker-compose.with-caddy.yml` default the dashboard to `backendrelay/dashboard:local`, and neither file can build it. Set `INSTANT_DASHBOARD_IMAGE` to an image you built from this repository. The `server` service in those files has a `build:` section. Run `docker compose ... build server` or set `INSTANT_SERVER_IMAGE`.
- **Restarts and health checks.** The shipped files don't define `restart` policies or health checks for `server` and `www`. Add `restart: unless-stopped` and a health check (`["CMD", "/usr/local/bin/instant-server", "healthcheck"]`) in an override file. Also copy the `server` `ulimits.nofile` setting from `docker-compose.yml` if you expect many concurrent clients.
- **PostgreSQL access.** `docker-compose.yml` and `docker-compose.local.yml` publish PostgreSQL only on `127.0.0.1:8890`. The production files (`docker-compose.yml`, `docker-compose.with-caddy.yml`) refuse to start without `POSTGRES_PASSWORD` and `INSTANT_DB_PASSWORD`. The local and test files fall back to `pass` (test: `instant-test`) because they only listen on loopback. The server connects as the non-superuser `INSTANT_DB_USER` role; `postgres` is the bootstrap superuser used by pgBackRest and recovery.
- **Password characters.** The compose files insert `POSTGRES_PASSWORD` and `INSTANT_DB_PASSWORD` into container settings and `DATABASE_URL` without escaping. Use only URL-safe characters: letters, digits, `.`, `_`, `~`, and `-`.

### Single host with Caddy

1. Point DNS for the API and dashboard hostnames (for example `api.example.com` and `dashboard.example.com`) at the host, and open ports 80 and 443. For production, provision the host first with [`self-hosting/provision/`](../self-hosting/provision/README.md): it covers the Cloudflare-fronted firewall, key-only SSH and Docker log limits.
2. Create `self-hosting/.env` from [`.env.example`](../self-hosting/.env.example) and run `chmod 600 .env`. Set at least these variables:

   | Variable | Example or meaning |
   | --- | --- |
   | `INSTANT_BACKEND_URL` | `https://api.example.com` |
   | `INSTANT_DASHBOARD_URL` | `https://dashboard.example.com` |
   | `BACKEND_DOMAIN`, `DASHBOARD_DOMAIN` | `api.example.com`, `dashboard.example.com` (read by the Caddyfile) |
   | `INSTANT_SECURE_COOKIES` | `true` |
   | `INSTANT_TRUSTED_PROXY_CIDRS` | `auto`, or the exact subnet of the Compose network. See [Trusted proxies](#tls-reverse-proxy-and-trusted-proxies). |
   | `INSTANT_TRUSTED_PROXY_HOST` | Optional. Proxy service name resolved by `auto` (default `caddy`). |
   | `POSTGRES_PASSWORD`, `POSTGRES_DB` | Superuser password and database name. Use a URL-safe password. |
   | `INSTANT_DB_USER`, `INSTANT_DB_PASSWORD` | Unprivileged application role the server connects as. Defaults to `instant`; the password is required in production. |
   | `CONNECTION_POOL_SIZE` | Main pool size per server process (default 20) |
   | `INSTANT_SERVER_IMAGE`, `INSTANT_DASHBOARD_IMAGE` | Your images, preferably `registry/repo@sha256:<digest>` |
   | `R2_ACCOUNT_ID`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY` | R2 account and a token scoped to both buckets |
   | `R2_FILES_BUCKET`, `R2_BACKUPS_BUCKET` | Two different buckets |
   | Email variables | See [Email](#email). |
   | `INSTANT_SUPERUSER_EMAIL` | Optional. This account can manage the whole deployment. |

3. Start PostgreSQL, apply migrations, then start everything:

   ```sh
   cd self-hosting
   docker compose -f docker-compose.with-caddy.yml up -d postgres
   docker compose -f docker-compose.with-caddy.yml run --rm server migrate up
   docker compose -f docker-compose.with-caddy.yml up -d
   curl -s https://api.example.com/health/system   # {"wal":"ok"}
   ```

## Building images

No prebuilt BackendRelay images are published. Build them and push them to your registry:

```sh
docker build -t registry.example.com/backendrelay/server:<version> \
  --build-arg BACKENDRELAY_VERSION="<version>" \
  --build-arg OCI_SOURCE=<your-repository-url> --build-arg OCI_REVISION="$(git rev-parse HEAD)" \
  server-rs
docker build -f client/Dockerfile.www -t registry.example.com/backendrelay/dashboard:<version> client
docker push registry.example.com/backendrelay/server:<version>
docker push registry.example.com/backendrelay/dashboard:<version>
docker inspect --format '{{index .RepoDigests 0}}' registry.example.com/backendrelay/server:<version>
```

`OCI_SOURCE` defaults to the BackendRelay repository URL
(`https://github.com/orenaksakal/backendrelay`), so override it when building
from a fork.
`BACKENDRELAY_VERSION` should be the release tag, for example `v2026.11.1`;
`OCI_REVISION` becomes the embedded git revision. Both are reported by
`instant-server --version` and `GET /health/version`. A build without
`BACKENDRELAY_VERSION` reports `<crate version>-dev`.

Both images build for the host architecture with a plain `docker build`. The
release flow publishes linux/amd64 and linux/arm64 indexes with signatures,
SBOM and provenance: run `bash self-hosting/release-images.sh` to build both
platforms locally (the host architecture is loaded for testing), `--push` to
publish, and see [`.github/RUNNER.md`](../.github/RUNNER.md) for the
tag-triggered workflow.

### Server image

`server-rs/Dockerfile` builds `cargo build --locked --release -p instant-server --no-default-features` on `rust:1.98.1-bookworm` and copies the binary to `/usr/local/bin/instant-server` on `debian:bookworm-slim`. The image runs as user `instant` (uid 10001), exposes 8888, and uses `docker-entrypoint.sh` as the entrypoint with the default command `serve`.

The binary's commands are `serve`, `healthcheck`, `generate-secrets [--rotate secrets.env]`, `reencrypt [--dry-run]`, `migrate [up | down [steps] | force <version>]`, `configure-byop <app-id>` (reads a PostgreSQL URL from stdin), `flags list | get <name> | set <name> <json> | unset <name>` (writes the deployment config app's flags), `audit` (read-only app-rules hygiene findings as JSON lines), and `failover`. See [operations.md](operations.md#key-rotation) for rotating the encryption, webhook, and rate-limit keys.

[`docker-entrypoint.sh`](../server-rs/docker-entrypoint.sh) behaves as follows:

- For `serve`, `migrate`, and `configure-byop`, it checks for `INSTANT_AEAD_KEYSET_JSON`, `INSTANT_WEBHOOK_KEYSET_JSON`, and `INSTANT_RATE_LIMIT_HMAC_KEY`. If any is missing from the environment, it loads `$INSTANT_CONFIG_DIR/secrets.env` (default `/home/instant/config`). If that file doesn't exist, it first creates it with `instant-server generate-secrets` and mode `0600`.
- The compose files mount the `server_secrets` volume at that path. **This volume holds the keyset that encrypts stored secrets.** Back it up with the database, because losing it makes that data unreadable. To rotate it without downtime, see [Key rotation](operations.md#key-rotation).
- `INSTANT_TRUSTED_PROXY_CIDRS=auto` makes the entrypoint resolve `INSTANT_TRUSTED_PROXY_HOST` (default `caddy`) with `getent ahosts` and trust only the resolved addresses as `/32` or `/128` routes. If the name does not resolve, it starts with no trusted proxies and prints a warning.

## PostgreSQL requirements

| Requirement | Details |
| --- | --- |
| Version | PostgreSQL 17. Tests use 17.11, and `recovery.py` refuses other major versions. |
| Logical decoding | `wal_level = logical`. The server streams changes over the replication protocol (`replication=database`). |
| `wal2json` output plugin | **Required.** The server creates permanent logical slots named `invalidator_*` with plugin `wal2json` and rejects slots that use any other plugin, such as `pgoutput` (`server-rs/crates/instant-pg/src/replication_slot.rs`). It requests `format-version` 2. Tests use wal2json 2.6. |
| `output_plugin_libraries` | Recent PostgreSQL 17 minors allow only `pgoutput` and `test_decoding` as logical output plugins by default, even for superusers. Add `wal2json` (the compose files pass `output_plugin_libraries=wal2json`); otherwise startup fails with `library "wal2json" may not be used as an output plugin`. |
| Slots and senders | Set `max_replication_slots` and `max_wal_senders` high enough for every server process. The compose files use 20 of each for the aggregator, per-process `invalidator_*` slots, and BYOP consumers. The server drops its own `invalidator_*` slot on graceful shutdown, removes idle slots from machines whose heartbeat is older than 60 seconds at startup, and keeps a five-minute grace on its hourly sweep. Monitor slot counts anyway, because every restart that cannot drop its slot leaves one behind. |
| WAL retention | Set `max_slot_wal_keep_size` to bound how much WAL an abandoned slot can pin; the compose files use `4GB`. Without a cap, a crashed process can fill the disk. When PostgreSQL invalidates a slot (`wal_status = 'lost'`) or replication fails with SQLSTATE `55000`, the server drops and recreates the slot, refreshes every local session, and for the aggregator rebuilds its sketches instead of crash-looping. |
| Role | The application role is a non-superuser with `REPLICATION` and ownership of the application database and `public` schema. That is enough to create the trusted extensions during the first `migrate up`; the role must not have `SUPERUSER`, `CREATEROLE`, or `CREATEDB`. `pg_hba.conf` must allow it to connect to the application database. Logical replication connections match on the database name. Check it with `SELECT rolsuper FROM pg_roles WHERE rolname = current_user;` (`f`). |
| Extensions | Migrations create `pgcrypto` (`32_pgcrypto`), `btree_gist`, and `pg_trgm` (`40_checked_indexes`). These ship with PostgreSQL contrib. |
| `pg_hint_plan` | Optional but recommended. The query planner emits `/*+ ... */` hints, which PostgreSQL ignores as comments when the extension isn't loaded. The compose files set `shared_preload_libraries=pg_hint_plan,pg_stat_statements`. |
| `pg_stat_statements` | Optional, for diagnostics. The compose files preload it; enable it once as superuser with `CREATE EXTENSION IF NOT EXISTS pg_stat_statements;`. |
| Memory | `shared_buffers` about 25% and `effective_cache_size` about 50–60% of the RAM PostgreSQL can count on. The production compose files default to `6GB` and `16GB` and accept `INSTANT_PG_SHARED_BUFFERS` / `INSTANT_PG_EFFECTIVE_CACHE_SIZE`; the local and test files default to `256MB` / `1GB` so laptops and CI are not starved. |
| `max_connections` | The compose files set `200`. Keep it at least `instances × (CONNECTION_POOL_SIZE + 41)`, with headroom for migrations and administration. |
| Slow-query logging | The compose files set `log_min_duration_statement=500ms`, so statements that take longer than half a second are logged. |
| Connections | Each server process opens a main pool of `CONNECTION_POOL_SIZE` connections, about three dozen single-connection auxiliary pools, and replication connections. Treat the `.env.example` rule (`max_connections` greater than `CONNECTION_POOL_SIZE` × instances) as a lower bound, and check `pg_stat_activity` after startup. |

The compose files build their own PostgreSQL 17 image from [`self-hosting/postgres/`](../self-hosting/postgres/), which installs wal2json 2.6, pg_hint_plan 1.7.1, and pgBackRest from the PGDG repository. The files start it with `wal_level=logical`, `max_replication_slots=20`, `max_wal_senders=20`, `max_slot_wal_keep_size=4GB`, `shared_buffers` of 25% of the available RAM, `effective_cache_size` of 50–60%, `max_connections=200`, `shared_preload_libraries=pg_hint_plan,pg_stat_statements`, `log_min_duration_statement=500ms`, `output_plugin_libraries=wal2json`, and `random_page_cost=1.1`. Set `INSTANT_POSTGRES_IMAGE` to a pushed digest in production instead of relying on the local build. Migration `151_triples_autovacuum` also tightens autovacuum on `triples` to a 2% vacuum and 1% analyze scale factor.

On a fresh data directory the image's init script creates the application role from `INSTANT_DB_USER` and `INSTANT_DB_PASSWORD`. It runs only during initialization, so an existing volume keeps the roles it already has.

### Upgrading an existing database to the split roles

Older compose files bootstrapped a superuser named `instant` and the server connected as it. Current files bootstrap `postgres` and connect as the unprivileged `INSTANT_DB_USER`. Convert an existing data directory once, as the old superuser, before starting the new stack:

```sh
docker compose exec -T postgres psql -U instant -d instant -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE postgres SUPERUSER LOGIN PASSWORD 'replace-with-POSTGRES_PASSWORD';
ALTER DATABASE instant OWNER TO instant;
GRANT CREATE ON DATABASE instant TO instant;
ALTER SCHEMA public OWNER TO instant;
ALTER ROLE instant NOSUPERUSER LOGIN REPLICATION;
SQL
```

Then set `POSTGRES_PASSWORD` to the new superuser password, `INSTANT_DB_USER=instant`, and `INSTANT_DB_PASSWORD` to the role's password in `.env` and restart. The server's connection must report `f` for `SELECT rolsuper FROM pg_roles WHERE rolname = current_user;`. Adjust the role and database names if the deployment used different values.

For managed PostgreSQL providers, see [managed-postgres.md](managed-postgres.md).

## Object storage

The server builds its S3 clients at startup and refuses to start without:

- `S3_ENDPOINT`
- `INSTANT_S3_REGION` (required whenever `S3_ENDPOINT` is set)
- `INSTANT_S3_ACCESS_KEY_ID` and `INSTANT_S3_SECRET_ACCESS_KEY`
- `S3_BUCKET` (user files)
- `S3_APP_BACKUPS_BUCKET` (per-app backups created from the dashboard's **Backups** tab)

Use two separate buckets. Optional settings:

- `S3_PUBLIC_ENDPOINT`: the endpoint browsers use in presigned URLs. Defaults to `S3_ENDPOINT`. Prefer a dedicated files domain (for R2, a custom domain such as `files.<domain>`) so user content is never served from the dashboard or API origin.
- `S3_VIRTUAL_HOST_STYLE=true|false`: path-style addressing by default.
- `INSTANT_S3_SESSION_TOKEN`.
- `INSTANT_STORAGE_MAX_UPLOAD_BYTES`: cap for a single upload, `104857600` (100 MiB) by default. Blank means unset.
- `INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS`: presigned download lifetime, `604800` (7 days) by default and clamped to 60–604800 seconds.

Uploads are hardened by default:

- Only `image/png`, `image/jpeg`, `image/gif`, `image/webp`, `image/avif`, `video/mp4`, `video/webm`, `audio/mpeg`, `audio/ogg`, `audio/wav` and `application/pdf` are stored with their declared type. Every other type, including `text/html`, `image/svg+xml` and JavaScript, is stored as `application/octet-stream`.
- The download disposition is decided at upload time: allowlisted media is stored with `Content-Disposition: inline`, and everything else with `Content-Disposition: attachment`. Presigned URLs carry no disposition override and simply serve the stored object headers, so signing a query or realtime refresh performs no object-store reads.
- Objects uploaded before this release keep their original headers and may still be served with their declared type. Serve files from a dedicated domain (`S3_PUBLIC_ENDPOINT`, such as the R2 custom domain `files.<domain>`) to contain that exposure.
- Uploads larger than `INSTANT_STORAGE_MAX_UPLOAD_BYTES` are rejected with `413` before being written, whether the size is declared in `Content-Length` or streamed.
- A per-app soft quota can be set with the config-app flags `storage-quota-bytes` (deployment default) and `storage-quota-bytes-by-app` (an object mapping app UUIDs to byte limits). The server compares the app's `$files` size aggregate against the quota and refuses new uploads with `403`; existing files are never deleted.

The shipped compose files map the `R2_*` variables to these settings, with region `auto`. For AWS S3 or another provider, edit the server's `S3_*` and `INSTANT_S3_*` entries in your copy of the compose file. The `${R2_ACCOUNT_ID:?}` checks fail interpolation before an override file is applied, so an override alone doesn't work.

Browsers fetch files directly from the bucket, so configure CORS for your dashboard origin. [`r2-cors.json`](../self-hosting/r2-cors.json) shows the rules: `GET`, `HEAD`, `PUT`, and `DELETE`; request headers `Content-Type`, `If-Match`, and `If-None-Match`; exposed headers `ETag` and `Content-Encoding`. For R2, [`r2-wrangler.sh`](../self-hosting/r2-wrangler.sh) uses your Wrangler login to create, configure, and verify both buckets. For example, `./r2-wrangler.sh ready https://dashboard.example.com` provisions both buckets, applies CORS, and runs a canary. If storage is served over HTTP from a different origin than the dashboard, set `INSTANT_STORAGE_ORIGIN` on the dashboard to that exact origin so image previews work.

## Email

Email delivers dashboard sign-in codes and app magic codes. Choose a provider with `INSTANT_EMAIL_PROVIDER`:

| Value | Required settings |
| --- | --- |
| `cloudflare` (the default in `.env.example`) | `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_EMAIL_API_TOKEN`, and `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL` on a domain enabled for Cloudflare Email Sending. If any is missing, the server refuses to start. |
| `postmark` | `POSTMARK_TOKEN`. `POSTMARK_ACCOUNT_TOKEN` is also needed to manage custom app sender signatures. |
| `sendgrid` | `SENDGRID_TOKEN` |
| `resend` | `RESEND_TOKEN`. The shipped compose files don't pass this variable to the server, so add it to the `server` environment yourself. |
| Empty | Picks a provider from whichever token is set. With no tokens, delivery is disabled and the server logs the codes. **Don't use this setting in production.** |

Configure sender identities with the `INSTANT_{DASHBOARD,APP,TEAM}_EMAIL_SENDER_{NAME,EMAIL}` variables.

### Onboarding a per-app sender domain (Cloudflare)

1. In the Cloudflare dashboard, open **Email Service → Email Sending** for the zone and enable the sending domain. The sender address an app uses must be on that domain.
2. Give the API token `CLOUDFLARE_EMAIL_API_TOKEN` the Email Sending Edit permission, plus Zone Read and Email Sending Read on that zone.
3. In the app's dashboard, an Admin or Owner sets the sender email and name, then uses the sender verification flow. The server confirms both the Cloudflare domain status and ownership of the address.
4. Until both checks pass, the app keeps sending from `INSTANT_APP_EMAIL_SENDER_EMAIL`. A verified domain is re-checked daily; if Cloudflare disables or removes it, the next check clears the verification and sends fall back to the platform sender.

Apps can only use domains verified for that app. Changing the sender address requires verifying ownership again.

## TLS, reverse proxy, and trusted proxies

- Terminate TLS in front of both services. Serve the API from one origin, such as `https://api.example.com` proxied to `server:8888`, and the dashboard from another, such as `https://dashboard.example.com` proxied to `www:3000`. Set `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL` to those origins.
- The proxy must pass WebSocket upgrades on `/runtime/session` and must not buffer streaming responses, because SDKs can also connect over SSE on `/runtime/sse`. Caddy handles both by default.
- Set `INSTANT_SECURE_COOKIES=true`. The server only marks cookies `Secure` when `INSTANT_BACKEND_URL` uses `https`. The compose files don't set `PRODUCTION` or `STAGING`. Setting either flag requires explicit `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL`; no defaults point at upstream infrastructure, so self-hosting doesn't need the flags.
- `INSTANT_TRUSTED_PROXY_CIDRS` is a comma-separated list of proxy CIDRs that may supply `X-Forwarded-For` or `X-Real-IP`. The server walks the forwarded chain only through trusted hops and uses the result for client IPs, including rate limiting and `request.ip` permission rules. Leave it empty when clients connect directly. Behind the bundled Caddy service, use `auto`; it resolves `INSTANT_TRUSTED_PROXY_HOST` (default `caddy`) to host routes instead of trusting the whole Docker network. For any other proxy, list that proxy's exact subnets. Never trust broad public ranges. When Cloudflare sits in front of Caddy, keep Caddy's `trusted_proxies` limited to Cloudflare's published ranges and read `CF-Connecting-IP`; the bundled `Caddyfile` does both.

### Edge rate limits

The server caps bodies and concurrent realtime sessions itself, but a public deployment should also reject abusive request rates at the edge before they consume server workers. Add a rate-limit rule for the unauthenticated and session-establishing paths:

- **Cloudflare** (Security → WAF → Rate limiting rules): match `http.request.uri.path matches "^/runtime/auth/" or http.request.uri.path matches "^/dash/" or http.request.uri.path matches "^/storage/"`, count by IP, allow roughly 120 requests per minute with a burst allowance, and block for at least one minute. Keep the limit high enough for dashboard polling and token refresh; the goal is to stop floods, not normal SDK traffic. Put the rule before any cache rule that would skip it.
- **Caddy** (`Caddyfile`): cap request bodies at the edge as a second line of defence. The server already returns 413 above 1 MiB for `/dash/*` and management routes, but rejecting at the proxy avoids forwarding the bytes:

  ```caddyfile
  @auth_and_dash path /runtime/auth/* /dash/*
  request_body @auth_and_dash {
      max_size 1MB
  }

  # App file uploads legitimately exceed the management limit.
  @storage path /storage/*
  request_body @storage {
      max_size 100MB
  }
  ```

  If you do not use Caddy, configure the equivalent body limit in your proxy. Do not buffer `/runtime/session`, `/runtime/sse` or `/dash/session_counts`; those routes must stay streaming.

## Dashboard

The `www` image serves the dashboard at `/dash` on port 3000. At startup, it writes `INSTANT_BACKEND_URL` (or `INSTANT_API_URI`) into `/publicSelfHostedVariables.js`. It exits if neither variable is a valid HTTP(S) URL.

- **Sign-in.** Users sign in with emailed codes, or with Google when `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID` and `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET` are both set. The server rejects only one of the pair. The OAuth redirect URI is `${INSTANT_BACKEND_URL}/dash/oauth/callback`.
- **Restricting access.** Dashboard signup is closed by default, so a new email cannot create an account. To allow people in:
  - `INSTANT_DASHBOARD_ALLOWED_EMAILS` (comma-separated) limits dashboard sign-in and sign-up, and listed emails may sign up even while signup is closed. The `INSTANT_SUPERUSER_EMAIL` address is always accepted. App user authentication is unaffected.
  - `INSTANT_DASHBOARD_SIGNUP=open` accepts any email and logs a startup warning. Leave it at `closed` for a private deployment.
  - `INSTANT_DASHBOARD_PRIVATE=true` with `INSTANT_DASHBOARD_GATEWAY_TOKEN` (32–128 characters from `[A-Za-z0-9_-]`) makes every dashboard, superadmin, and platform-OAuth route require the `x-instant-dashboard-gateway-token` header. A private gateway you operate must inject that header. Runtime and Admin SDK routes stay public.
- **Ephemeral apps.** `POST /dash/apps/ephemeral` is disabled by default and returns 403. Set `INSTANT_EPHEMERAL_APPS=true` only if you operate the upstream ephemeral-app flow; these apps are deleted after 14 days unless claimed.

## Migrations and upgrades

The server never applies migrations. At startup, it checks the `schema_migrations` table and the recorded checksums. It refuses to serve when the schema is behind the binary (the error tells you to run `migrate up`), when the schema is newer than the binary by anything other than a run of additive migrations, when a migration is marked dirty, or when the checksums don't match. Migrations are embedded in the binary from `server-rs/crates/instant-server/migrations/`, and concurrent runs are serialized with an advisory lock.

To upgrade:

1. Back up the database and the `server_secrets` volume. See [Backups and recovery](#backups-and-recovery).
2. Build or pull the new images, and set `INSTANT_SERVER_IMAGE` and `INSTANT_DASHBOARD_IMAGE` to the new digests.
3. Apply migrations with the new server image: `docker compose -f <file> run --rm server migrate up`.
4. Restart `server`, then `www`.

An older binary accepts a newer schema only while every migration in between is marked additive; otherwise it refuses to serve. To roll back across a non-additive migration, run `migrate down <steps>` with the newer image before you start the older one. `migrate force <version>` rewrites version metadata after you've repaired a failed migration by hand. [`run-compose-image-rollback.sh`](../self-hosting/run-compose-image-rollback.sh) shows how this repository tests image rollback, and [`run-upgrade-rollback-test.sh`](../self-hosting/run-upgrade-rollback-test.sh) exercises migrate up, the previous image against the candidate schema, and migrate down across two images; [development.md](development.md#schema-changes-and-rollbacks) describes how to mark a migration additive.

## Backups and recovery

A complete backup has three parts:

- **The PostgreSQL database.** Logical replication slots aren't included in a dump, and the server recreates them.
- **The `server_secrets` volume** (`/home/instant/config`), which holds the encryption keyset. Keep the separate age-encrypted copy from [Secrets](operations.md#secrets) as well: the `recovery.py` bundle includes this directory, but it shares a bucket and credentials with the database copy.
- **Both object-storage buckets.** Back these up with your storage provider, such as bucket replication or versioning. The tools below don't copy them.

For managed PostgreSQL, also use the provider's point-in-time recovery.

**Continuous WAL archiving and physical backups.** The PostgreSQL image includes pgBackRest, and the base compose files expose it through `PGBACKREST_*` environment variables. Archive mode is **off** by default (`INSTANT_PG_ARCHIVE_MODE=off`); an enabled `archive_command` without a working repository retains every WAL segment. Join a deployment with a second, independent copy of the database: repo 1 is Cloudflare R2 and repo 2 is an optional S3-compatible provider such as Backblaze B2 or Hetzner Object Storage. Weekly full backups run Sunday at 02:30 UTC, differential backups run Monday to Saturday at 02:30 UTC, and both repositories keep 14 days. The image's entrypoint makes the named `pgbackrest-spool` volume writable, and `archive_timeout=60` bounds the RPO on an idle database. See [`self-hosting/backup/README.md`](../self-hosting/backup/README.md) for setup, inspection, point-in-time restore, and the `--target-action=promote` flow.

[`recovery.py`](../self-hosting/recovery.py) automates encrypted off-host backups for a single Docker host:

- **`backup --config <file>`.** See [`recovery-config.example.json`](../self-hosting/recovery-config.example.json). The command:
  1. Runs `pg_dump --format=custom` of PostgreSQL 17 and archives the server-secrets mount and a configuration directory. `pg_dump` reads one MVCC snapshot, so the server keeps serving writes during the backup; the manifest records `consistency: "mvcc-snapshot"`.
  2. Records the running service and database image identities in the manifest. It does **not** save images on every run.
  3. Encrypts the bundle with [`age`](https://age-encryption.org) for `ageRecipients`.
  4. Uploads it with your `uploadCommand`, verifies the off-host checksum with `verifyOffhostCommand`, and keeps `keepLocal` bundles locally.

  It requires digest-pinned images, the deployed source revision, and at least 5 GiB of free disk. It can write a node-exporter textfile metric that the `BackupStale` alert reads. Set `healthcheckUrl` to ping healthchecks.io after each run (`<url>` on success, `<url>/fail` on failure); ping failures never fail the backup. Systemd units for a twice-daily schedule are in [`monitoring/`](../self-hosting/monitoring/) (`backendrelay-backup.{service,timer}` and `backendrelay-backup-resume.service`). They assume the repository is at `/opt/backendrelay` and the configuration is at `/etc/backendrelay/backup.json`.
- **`snapshot-images --config <file>`.** Saves the candidate, rollback and database images once per deploy with `docker image save`. It encrypts the archive to `<backupRoot>/images/<sha256-of-sorted-image-ids>.tar.age`, uploads and verifies it with the same commands as `backup`, and skips the work when that exact snapshot already exists. A later `backup` records the snapshot file name in `imagesBundle` when its image IDs match, but bundles never contain images.
- **Remote databases.** `databases.primary` is normally a container name. For managed PostgreSQL, set it to `{"dsnFile": "/absolute/path"}` instead. The file must have mode `0600` and contain one `postgresql://` URL. `pg_dump`, `pg_restore --list` and the version/identity queries then run locally with a PostgreSQL 17 client; the password is passed only in the child's `PGPASSWORD` environment variable and never appears in argv or errors.
- **`restore --bundle <file> --identity-file <age-key> --target-config <file> [--images <file>]`.** See [`restore-config.example.json`](../self-hosting/restore-config.example.json). It restores into an empty, explicitly disposable target. The database container must carry the label `backendrelay.restore.disposable=true`, run PostgreSQL 17, and have no tables. The command verifies checksums and image identities, runs `pg_restore`, writes an image override, and runs your start and verify commands. Bundles contain no images, so pass `--images` with the encrypted snapshot produced by `snapshot-images` (for example, downloaded from off-host storage); its file name must match the bundle's `imagesBundle`, and it is decrypted with the same `--identity-file`. Legacy schema-3 bundles that contain `images.tar` still load it. Otherwise the exact image IDs must already exist locally, and the error names any missing IDs and the expected snapshot file name. It refuses bundles older than 24 hours so it remains safe as a routine drill; for real recovery add `--allow-older-than <days>` (integer ≥ 1), which prints a warning and accepts up to that age. Future-dated bundles are always refused.

  To recover from an older bundle by hand, decrypt it with `age --decrypt`, extract the tar archive (`primary.dump`, `server-secrets.tar`, `configuration.tar`, `manifest.json`, `checksums.json`, plus `images.tar` in legacy schema-3 bundles), and `pg_restore` `primary.dump` into an empty PostgreSQL 17 database.
- **`resume-writers --config <file>`.** Restarts writers that an interrupted backup from an older version of this tool left stopped. Current backups never stop writers.
- **Per-app backups.** The nightly per-app worker runs in the `production` profile. Set `INSTANT_NIGHTLY_APP_BACKUPS=true` to enable it in any profile, or `false` to disable it in production. Apply the 30-day expiry rule in [`r2-backup-lifecycle.json`](../self-hosting/r2-backup-lifecycle.json) to the backups bucket with `./self-hosting/r2-wrangler.sh configure-lifecycle` (from a machine with Wrangler OAuth access).

[`recovery-drill.mjs`](../self-hosting/recovery-drill.mjs) isn't a backup tool. It injects restart, crash, and database-outage faults into the disposable project that `run-single-server-restart-acceptance.sh` creates.

## Monitoring and health

**Health endpoints:**

- `GET /health/system` on the server returns `200 {"wal":"ok"}` when the WAL pipeline is healthy and `500 {"wal":"error"}` otherwise.
- `GET /health/version` returns `{"version":"<tag>","git_sha":"<revision>","migration_version":<n>}` for the running binary. Use it to confirm which release and schema a deployment actually runs.
- `instant-server --version` prints the same version and revision, and the startup log records both.
- `instant-server healthcheck` calls `/health/system` on `127.0.0.1:${PORT:-8888}`. Use it as the container health check.
- The dashboard container answers `GET /` on port 3000.

**Metrics.** Set `INSTANT_METRICS_ADDR` to a loopback address, such as `127.0.0.1:9091`, to serve Prometheus metrics at `/metrics`. The server rejects non-loopback addresses unless `INSTANT_METRICS_ALLOW_NONLOOPBACK=true` is also set, which additionally allows private RFC1918, IPv6 unique-local, or unspecified bind addresses. Use that opt-in only for a private container network such as the monitoring overlay; public addresses stay rejected.

**Monitoring stack.** [`docker-compose.monitoring.yml`](../self-hosting/docker-compose.monitoring.yml) is an overlay written to merge after the base compose file. It sets `INSTANT_METRICS_ADDR` and adds:

- Prometheus on the compose network, with 15-day or 5 GB retention and the rules in [`monitoring/alerts.yml`](../self-hosting/monitoring/alerts.yml).
- Alertmanager, with Pushover receivers (priority 2 for critical, priority 0 for warnings) and an external dead-man receiver for the `Watchdog` heartbeat.
- Grafana on `127.0.0.1:3001`, with the provisioned BackendRelay dashboard, including realtime and per-app rows, and Loki as a second datasource.
- Loki on the private compose network, keeping container logs for 14 days on a named volume, and Grafana Alloy shipping them through a read-only Docker API socket proxy.
- node-exporter, with a textfile directory for backup metrics.
- postgres-exporter, with a DSN from `POSTGRES_EXPORTER_DSN` and the statement, capacity and slot queries in [`monitoring/postgres-queries.yml`](../self-hosting/monitoring/postgres-queries.yml).
- Bugsink behind the optional `errors` profile, bound to `127.0.0.1:8000` with SQLite on a named volume, for Sentry-protocol error tracking. See [Error tracking](operations.md#error-tracking).
- The realtime canary behind the optional `canary` profile, probing the public API every minute and exposing propagation metrics on `canary:9465`. See [External monitoring and canary](operations.md#external-monitoring-and-canary).

Required inputs:

- Digest-pinned images: `PROMETHEUS_IMAGE`, `ALERTMANAGER_IMAGE`, `GRAFANA_IMAGE`, `NODE_EXPORTER_IMAGE`, `POSTGRES_EXPORTER_IMAGE`, `LOKI_IMAGE`, `ALLOY_IMAGE`, and `DOCKER_SOCKET_PROXY_IMAGE`; `BUGSINK_IMAGE` for the `errors` profile and `CANARY_IMAGE` for the `canary` profile.
- Absolute file paths: `PUSHOVER_USER_KEY_FILE`, `PUSHOVER_TOKEN_FILE`, `DEADMAN_WEBHOOK_FILE`, `GRAFANA_PASSWORD_FILE`, `POSTGRES_MONITOR_PASSWORD_FILE`, and `MONITORING_TEXTFILE_DIR`; `BUGSINK_SECRET_KEY_FILE` for the `errors` profile and `CANARY_ADMIN_TOKEN_FILE` for the `canary` profile.
- `POSTGRES_EXPORTER_DSN`, for example `postgres:5432/instant?sslmode=disable`. The exporter user defaults to `backendrelay_monitor`; set `POSTGRES_EXPORTER_USER` to change it.

Setup notes:

- Create the exporter role with [`monitoring/postgres-role.sql`](../self-hosting/monitoring/postgres-role.sql) (`backendrelay_monitor`, granted `pg_monitor`).
- The shipped PostgreSQL image creates the `pg_stat_statements` extension on a fresh data directory. On an existing volume, run `CREATE EXTENSION IF NOT EXISTS pg_stat_statements;` once in the application database; the library is already preloaded through `shared_preload_libraries`.
- The default exporter queries discover replication slots by name prefix: `invalidator_<uuid>` for each server process and the shared `aggregator` slot. No per-deployment slot names are needed, and the default expects one invalidator replica. A cluster with more replicas renders `monitoring/slot_queries.py --expected-replicas <n> --output <yml>` and points `POSTGRES_MONITOR_QUERIES_FILE` at the result.
- Prometheus waits for the server's health check, so a base file without one (see [Deployment options](#deployment-options)) needs a health check added first.

The alerts cover availability, 5xx rate, p95 latency, pool and admission pressure, realtime refresh backlog and rejections, receive wait, WebSocket exits, result-cache budget pressure, panics, scheduled-task failures, email and webhook delivery, WAL retention above 1 GiB, missing, invalid, inactive, unhealthy, or stalled slots, connections against `max_connections`, long or idle transactions, deadlocks, `triples` dead tuples, database growth, disk headroom, backup age, failing WAL archiving, and failed alert delivery.

## Sizing

A 4-vCPU/8 GB server plus a small PostgreSQL instance comfortably handles early-stage traffic. See [performance.md](performance.md) for measured capacity.

## Production checklist

- The host is rebuilt from a clean Ubuntu 24.04 image and provisioned with [`self-hosting/provision/host-bootstrap.sh`](../self-hosting/provision/host-bootstrap.sh): key-only SSH, `ufw`, Docker log limits and unattended security updates. The Hetzner Cloud Firewall is the authoritative filter because Docker-published ports bypass `ufw`; the ordered owner steps are in [`self-hosting/provision/README.md`](../self-hosting/provision/README.md).
- Images are built from this repository and referenced by digest, including PostgreSQL.
- `POSTGRES_PASSWORD` and `INSTANT_DB_PASSWORD` are strong and URL-safe. The server connects as the non-superuser `INSTANT_DB_USER` (`SELECT rolsuper ...` returns `f`), and PostgreSQL isn't reachable from the internet.
- `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL` use `https`, `INSTANT_SECURE_COOKIES=true`, and `INSTANT_TRUSTED_PROXY_CIDRS` matches only your proxy.
- A real email provider is configured. Dashboard access is restricted if you need it.
- `migrate up` runs before every server upgrade.
- The database, the `server_secrets` volume, and both buckets are backed up, and restores are tested. The keysets also have an independent age-encrypted copy in R2 and offline ([Secrets](operations.md#secrets)).
- `/health/system` and replication-slot WAL retention are monitored.
- Run `instant-server audit` monthly (and after onboarding apps): it reports a missing or unlocked config app, apps with no or empty rules, apps whose `$default` allows everything, apps without an owner, and stale ephemeral apps, and exits non-zero on errors.
