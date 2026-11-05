# Server configuration reference

This page covers configuration for the BackendRelay server binary, `instant-server` ([`server-rs/crates/instant-server`](../server-rs/crates/instant-server)). It lists the command-line interface, every environment variable the binary reads, the checks the binary runs at startup, and how to size the connection pool.

Client applications need none of this. They use the standard InstantDB SDKs (`@instantdb/*`) with the SDK `apiURI` and `websocketURI` options pointed at the server's public URL (`INSTANT_BACKEND_URL`).

## Contents

- [How configuration is loaded](#how-configuration-is-loaded)
- [Command-line interface](#command-line-interface)
- [Environment profile](#environment-profile)
- [Environment variables](#environment-variables)
  - [Listener and public URLs](#listener-and-public-urls)
  - [Database and connection pool](#database-and-connection-pool)
  - [Required secrets](#required-secrets)
  - [Realtime, WAL and replication](#realtime-wal-and-replication)
  - [Object storage (S3-compatible)](#object-storage-s3-compatible)
  - [Email](#email)
  - [Dashboard access, cookies and OAuth](#dashboard-access-cookies-and-oauth)
  - [Proxies](#proxies)
  - [Observability](#observability)
  - [Integrations](#integrations)
  - [Runtime and library variables](#runtime-and-library-variables)
  - [Container entrypoint variables](#container-entrypoint-variables)
  - [Test-only variables](#test-only-variables)
- [Validation rules](#validation-rules)
- [Connection pool sizing](#connection-pool-sizing)
- [Minimal production configuration](#minimal-production-configuration)

## How configuration is loaded

- The binary reads configuration only from process environment variables. It does not read a configuration file or a `.env` file. The Docker image's entrypoint is the one exception: it can source a generated secrets file before starting the binary (see [Container entrypoint variables](#container-entrypoint-variables)).
- Configuration is read once, at startup. If it is invalid, the process prints `instant-server failed: <reason>` to stderr and exits with status 1. Error messages name the variable but never print its value.
- Profile flags (`STAGING`, `PRODUCTION`, `TEST`) and `INSTANT_SECURE_COOKIES` are on only when the value is exactly the lowercase string `true`. Any other value, including an empty one, counts as off.
- Variables documented as `true`/`false` accept only those two exact values.
- An empty or whitespace-only value counts as unset for optional strings such as tokens, names, bucket names and email addresses. The following variables are validated exactly as given, so an empty value **fails startup**. Leave them unset instead of setting them to an empty string: `PORT`, `DATABASE_URL`, `INSTANT_BACKEND_URL`, `INSTANT_DASHBOARD_URL`, `CONNECTION_POOL_SIZE`, `INSTANT_SKETCH_BATCH_*`, `S3_ENDPOINT`, `S3_PUBLIC_ENDPOINT`, `S3_VIRTUAL_HOST_STYLE`, `INSTANT_METRICS_ADDR`.
- Commands differ in what they read. `--help`, `--version`, `generate-secrets` and `healthcheck` read no server configuration. Every other command loads and validates the full server configuration. `serve` and `configure-byop` also load the [required secrets](#required-secrets).

## Command-line interface

```text
instant-server [serve | healthcheck | failover | generate-secrets | configure-byop <app-id> | flags list | get <name> | set <name> <json> | unset <name> | audit | migrate [up | down [steps] | force <version>]]
```

Source: [main.rs].

| Command | What it does |
| --- | --- |
| *(no arguments)*, `serve` | Starts the HTTP and WebSocket server on `0.0.0.0:<PORT>` (IPv4 only). Before it binds the listener, it connects every database pool, **verifies** that migrations and the system catalog are current, and builds the object store. On SIGINT or SIGTERM it withdraws readiness, drains live sessions (see [Shutdown drain](#shutdown-drain)), then shuts down gracefully with a 30-second grace period. |
| `migrate`, `migrate up` | Applies all pending embedded migrations while holding a PostgreSQL advisory lock, then creates or updates the system catalog. Prints `database schema <from> -> <to>; migration versions: [...]`. |
| `migrate down [steps]` | Rolls back `steps` migrations (default `1`). Server startup never runs this. |
| `migrate force <version>` | Rewrites the recorded migration version and clears the dirty flag **without running any SQL**. Use it after you have inspected and repaired a failed migration by hand. `0` clears the version row. Fails if `<version>` is newer than the binary's latest migration. |
| `flags list` | Prints one `setting<TAB>json-value` line for every flag in the config app. |
| `flags get <name>` | Prints the JSON value of one config app flag. Exits with status 1 when the flag is unset. |
| `flags set <name> <json>` | Creates or replaces one config app flag. The value must be valid JSON. Writes go through the native transaction path so WAL consumers invalidate cached policy reads. |
| `flags unset <name>` | Deletes the config app flag entity. A missing flag is a no-op. |
| `audit` | Reads the app rules and ownership state without changing it and prints one JSON finding per line, then a summary. Reports a missing or unlocked config app, apps with no or empty rules, apps whose `$default` allows everything, apps without an owner, and stale ephemeral apps. Exits with status 1 when there is any error finding. |
| `failover` | Runs the leader side of an online failover from the primary database (`DATABASE_URL`) to the next database (`NEXT_DATABASE_URL`). Requirements: both databases are fully migrated, they are different databases, and the `failing-over` and `disable-aggregator` toggles are enabled in the database's flag configuration. Running `serve` processes that have the same next database configured act as participants. With no next database configured, `failover` exits with the usage error. |
| `configure-byop <app-id>` | Reads one PostgreSQL URL from **standard input**, encrypts it with `INSTANT_AEAD_KEYSET_JSON`, and stores it as the app's bring-your-own-Postgres connection string. Reading from stdin keeps credentials out of the process argument list. Fails if the app does not exist. |
| `generate-secrets` | Prints fresh `INSTANT_AEAD_KEYSET_JSON`, `INSTANT_WEBHOOK_KEYSET_JSON` and `INSTANT_RATE_LIMIT_HMAC_KEY` assignments, single-quoted for a shell or `.env` file. |
| `healthcheck` | Sends `GET /health/system` to `127.0.0.1:<PORT>`. Exits 0 only if the response is HTTP 200 with JSON `{"wal":"ok"}` within 5 seconds. Reads only `PORT` and defaults to `8888` if `PORT` is unset or invalid. It does **not** consider the test-profile default port. Intended for container health checks. |
| `-h`, `--help` | Prints the usage line. |
| `-V`, `--version` | Prints `instant-server <version>`. |

Any other arguments print the usage line as an error and exit with status 1.

### Config app flags

Migration 146 seeds the deployment-wide config app
(`24a4d71b-7bb2-4630-9aee-01146af26239`) with the system catalog user as its
creator, no admin token, no members, and a deny-all rules document. The app ID
is reserved: dashboard, ephemeral, get-a-db, superadmin and restore creation
paths reject it. The old dashboard's Deployment Settings page wrote flags with
the config app admin token, which the migration deletes. Operators use
`instant-server flags ...` until the new dashboard operator page ships.

Recognized `flags` settings:

| Setting | Value |
| --- | --- |
| `always-materialize-attr-ids` | Array of attribute UUIDs or `[uuid, label]` pairs. |
| `row-hint-threshold` | Number. |
| `disable-hint-query-hashes`, `use-hint-query-hashes`, `enable-row-hints-query-hashes`, `disable-row-hints-query-hashes` | Array of query hashes. |
| `disable-row-hints-apps` | Array of app UUIDs. |
| `coarse-topics-apps`, `refresh-throttled-apps` | Array of app UUIDs. |
| `refresh-throttle-ms` | Number of milliseconds. |
| `combine-transacts` | Boolean. |
| `team-emails` | Array of `{"email": "..."}` objects that grant dashboard operator rights. |
| `dashboard-signups` | `{"mode": "open" \| "restricted" \| "closed", "allowedEmails": [...]}`. When unset, `INSTANT_DASHBOARD_SIGNUP` decides, defaulting to `closed`. |
| `magic-code-rate-limit-per-hour` | Positive integer. |
| `magic-code-daily-limit-per-app` | Positive integer daily send cap for every app. When unset, `INSTANT_MAGIC_CODE_DAILY_LIMIT_PER_APP` decides, defaulting to `1000`. |
| `magic-code-rate-limit-per-ip-hour` | Positive integer hourly send cap per client IP on the public runtime route. When unset, `INSTANT_MAGIC_CODE_RATE_LIMIT_PER_IP_HOUR` decides, defaulting to `30`. |
| `magic-code-verify-limit-per-ip-hour` | Positive integer hourly verify cap per client IP on the public runtime route. When unset, `INSTANT_MAGIC_CODE_VERIFY_LIMIT_PER_IP_HOUR` decides, defaulting to `60`. |
| `ephemeral-apps-enabled` | Boolean. When unset, `INSTANT_EPHEMERAL_APPS` decides, defaulting to `false`. |
| `app-metric-slugs` | Object mapping app UUIDs to the slug used as the `app` metric label: lowercase letters, digits and `-`, at most 32 bytes, and never `other`. An app without a slug uses the first eight characters of its UUID; only the first 100 resolved apps keep their own label and the rest report as `other`. Read when the routes are assembled, so restart the server after a change. |
| `on-demand-backup-max-triples` | Non-negative integer; `0` removes the limit. |
| `on-demand-backup-app-rate-limit`, `on-demand-backup-ip-rate-limit` | Rate-limit policy JSON, or `null`. |
| `enable-wal-entity-log-apps-map` | Object mapping app UUIDs to `true`. |
| `backup-skip-app-ids` | Array of app UUIDs. |
| `smokescreen-whitelist-ips` | Array of IP addresses or CIDRs allowed past the webhook SSRF guard. |
| `storage-sweeper-max-loops` | Number of storage sweep loops per run. |
| `storage-quota-bytes` | Positive integer soft storage quota, in bytes, for every app. Uploads that would push the app's `$files` size aggregate over the quota are refused with `403`. Existing files are never deleted. Unset means unlimited. |
| `storage-quota-bytes-by-app` | Object mapping app UUIDs to positive integer byte quotas. An entry overrides `storage-quota-bytes` for that app. |

Toggles use a separate `toggles` namespace and are not written by `flags`.
The legacy `rate-limited-apps` collection uses its own etype, and this server does
not implement `app-proxy-targets`; neither is part of the `flags` namespace.

### Migrations are never applied by `serve`

Serving verifies migrations but never applies them. If the database schema is behind the binary, `serve` refuses to start with an error like:

```text
database schema version <current> is behind this binary's required version <required>; run the explicit `instant-server migrate up` command before starting the server
```

Startup also fails if the database is newer than the binary, a migration is marked dirty, the migration metadata is incomplete, or an embedded migration no longer matches its recorded checksum ([migrations.rs]). Before you start a new server version, run `instant-server migrate up` against the same database. From a source checkout, the equivalent is `cargo run --locked -p instant-server -- migrate up`, run from `server-rs/`. `migrate` validates the full server configuration too, so it needs the same environment as `serve` apart from the secrets.

## Environment profile

Three flags select a profile. The profile changes several defaults and some background behavior ([config.rs]).

| Profile | Selected when | "Hosted" |
| --- | --- | --- |
| `staging` | `STAGING=true` (wins over every other flag) | yes |
| `production` | `PRODUCTION=true`, and `STAGING` is not `true` | yes |
| `test` | `TEST=true`, and neither of the above is set | no |
| `development` | none of the flags is `true` (the default) | no |

What the profile changes:

- **Defaults:** `PORT` and `CONNECTION_POOL_SIZE`. `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL` default to localhost in `development`/`test` and are **required** in `production`/`staging`. `S3_BUCKET` is required whenever `S3_ENDPOINT` is set. `S3_APP_BACKUPS_BUCKET` and `WAL_HISTORY_BUCKET_NAME` have no defaults. Nothing defaults to upstream InstantDB infrastructure.
- **Secure cookies:** turned on automatically in every profile except `development` when `INSTANT_BACKEND_URL` uses `https`.
- **`staging` and `production`:** Periodic gauge events are written to stdout.
- **`production` only:** nightly app backups (set `INSTANT_NIGHTLY_APP_BACKUPS` to override the profile in either direction), an extra per-app storage access check, and different intervals for background jobs.
- **`development` only:** `HOSTNAME` is used as the key prefix for archived WAL history.
- **`test`:** the default port is `8886`. There are no default backup or WAL-history buckets. Test harnesses use this profile.

The self-hosting Compose files in [`self-hosting/`](../self-hosting) set none of these flags. They run with the `development` profile and set every profile-dependent value explicitly. If you set `PRODUCTION` or `STAGING`, you must also set `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL`.

## Environment variables

Unless a row says otherwise, the source file is under `server-rs/crates/instant-server/src/`.

### Listener and public URLs

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `PORT` | Optional | `8886` in the `test` profile and `8888` in all others | Integer, 0–65535 | TCP port of the HTTP/WebSocket listener. The bind address is always `0.0.0.0`. | [config.rs], [main.rs] |
| `INSTANT_BACKEND_URL` | **Yes in `production`/`staging`**; otherwise yes for any non-local deployment | `development`/`test`: `http://localhost:<port>`. `production`/`staging`: none; startup fails with `INSTANT_BACKEND_URL is required in the production and staging profiles`. | Absolute `http(s)` URL with a host. A trailing `/` is removed. | Public origin of this server. Used to build OAuth callback URLs (including `<url>/dash/oauth/callback`), storage and webhook URLs, and the webhook signing identity. An `https` URL is a precondition for secure cookies. | [config.rs] |
| `INSTANT_DASHBOARD_URL` | **Yes in `production`/`staging`**; otherwise yes if you run the dashboard | `development`/`test`: `http://localhost:3000`. `production`/`staging`: none; startup fails with `INSTANT_DASHBOARD_URL is required in the production and staging profiles`. | Absolute `http(s)` URL. A trailing `/` is removed. | Public origin of the dashboard. Used as the target of redirects and links back to the dashboard, and as the only origin allowed to call `/platform/oauth/claim` cross-origin. | [config.rs] |

### Database and connection pool

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `DATABASE_URL` | Yes, in practice | `postgresql://localhost:5432/instant` | URL starting with `postgres://` or `postgresql://`. A leading `jdbc:` is stripped. Must parse as a `tokio-postgres` connection string. Parameters such as `sslmode` are honored. | Primary application database. TLS connections verify the server certificate against the bundled WebPKI root store. There is no setting for a custom CA. | [config.rs], [postgres_pool.rs] |
| `CONNECTION_POOL_SIZE` | Optional | `400` in `staging`/`production`; `20` otherwise | Integer > 0 | Maximum size of the main database pool, which is split across workload roles (see [Connection pool sizing](#connection-pool-sizing)). Also the maximum size of the replica pool on `NEXT_DATABASE_URL`. | [config.rs], [postgres_pool.rs] |
| `INSTANT_HISTORY_RETENTION_DAYS` | Optional | `30` | Integer >= 1 | Days of webhook/entity history kept in the 30-day `history` and `webhook_events` partitions. Values that are not positive integers keep the default. | [partition_retention.rs] |
| `INSTANT_TRANSACTION_RETENTION_DAYS` | Optional | `30` | Integer >= 1 | Days of `transactions` marker rows kept before the custodian deletes them in batches. Values that are not positive integers keep the default. | [custodian.rs] |
| `NEXT_DATABASE_URL` | Optional | unset | Same format as `DATABASE_URL`. Blank means unset. | Target database for `failover`. When it is set and differs from `DATABASE_URL`, `serve` also opens a second pool of up to `CONNECTION_POOL_SIZE` connections to it, used for realtime replica reads when the failover gate permits them, and runs a failover participant. | [config.rs], [main.rs], [production_routes.rs] |
| `DATABASE_CLUSTER_ID` | Not supported | unset | Aurora cluster identifier | Unsupported: the self-hosted server always uses `DATABASE_URL`. Setting this fails startup. | [config.rs], [main.rs] |
| `NEXT_DATABASE_CLUSTER_ID` | Not supported | unset | Aurora cluster identifier | Unsupported: the self-hosted server always uses `NEXT_DATABASE_URL`. Setting this fails startup. | [config.rs], [main.rs] |

### Required secrets

`serve` and `configure-byop` need these. Generate them once with `instant-server generate-secrets` and share the same values with every instance that uses the same database. Keep the values stable: if `INSTANT_AEAD_KEYSET_JSON` is lost or replaced, stored OAuth client secrets and BYOP connection strings can no longer be decrypted.

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_AEAD_KEYSET_JSON` | **Yes** | none | Cleartext Tink JSON keyset with an AES-GCM key. | Encrypts app OAuth client secrets and BYOP connection strings at rest. | [production_routes.rs], [self_hosted_secrets.rs] |
| `INSTANT_WEBHOOK_KEYSET_JSON` | **Yes** | none | Tink JSON keyset with an Ed25519 private key | Signs outgoing webhook payloads. The public keys are published as a JWKS. | [production_routes.rs], [self_hosted_secrets.rs] |
| `INSTANT_RATE_LIMIT_HMAC_KEY` | **Yes** | none | Non-empty string. The raw string bytes are the key; `generate-secrets` emits Base64 text of 32 random bytes. | HMAC-SHA256 key used to derive the keys of the durable rate-limit buckets stored in PostgreSQL. | [production_routes.rs] |
| `INSTANT_AEAD_KEYSET_KMS_KEY_URL` | Not supported | unset | `aws-kms://arn:aws:kms:<region>:...` | Unsupported: AWS KMS-encrypted keysets are unavailable. Setting this fails startup. | [production_routes.rs] |
| `INSTANT_WEBHOOK_KEYSET_KMS_KEY_URL` | Not supported | unset | `aws-kms://...` | Same as above, for the webhook keyset. | [production_routes.rs] |

### Realtime, WAL and replication

Realtime invalidation streams changes from PostgreSQL logical replication slots through the `wal2json` output plugin. The database therefore needs `wal_level=logical`, free replication slots and WAL senders, `wal2json` installed, and a database role that is allowed to create logical replication slots. See [`self-hosting/docker-compose.yml`](../self-hosting/docker-compose.yml) for a working PostgreSQL configuration.

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS` | Optional | `64` | Integer, 1–65536 | Maximum number of WAL transactions grouped into one write of the derived statistics ("sketches") aggregated from the WAL. This bounds only how statistics are persisted, not the durability of application transactions. | [config.rs], [replication.rs] |
| `INSTANT_SKETCH_BATCH_MAX_WAL_BYTES` | Optional | `1048576` (1 MiB) | Integer, 1–67108864 (64 MiB) | Maximum WAL bytes per statistics batch. | [config.rs], [replication.rs] |
| `INSTANT_SKETCH_BATCH_MAX_WAIT_MS` | Optional | `50` | Integer, 1–10000 | Maximum time a statistics batch stays open before it is written. Larger batches (for example `1024` / `16777216` / `1000`) reduce write load on busy servers, but statistics are persisted less often. | [config.rs], [replication.rs] |
| `INSTANT_WAL_SINK_REFRESH_DEADLINE_MS` | Optional | `10000` (10 s) | Positive integer milliseconds | Deadline for one session's query refresh inside a WAL window. A refresh that exceeds it fails for that session (logged with the app and window id, counted by `backendrelay_wal_sink_refresh_timeouts_total`), while the rest of the window completes, so one stalled query cannot hold later windows behind it. The affected session then retries on its own after a backoff of 1 s, doubling per consecutive timeout up to 30 s and resetting on success (`backendrelay_wal_sink_forced_resyncs_total`). | [runtime_realtime.rs] |
| `WAL_HISTORY_STORAGE` | Optional | unset | Only the exact value `pg` has any effect | `pg` disables the S3 WAL-history archive: no WAL-history bucket is configured, and `WAL_HISTORY_BUCKET_NAME` is ignored. Replaying a legacy history row that was archived to S3 then returns an error. The self-hosting Compose files set `pg`. | [config.rs], [production_platform.rs] |
| `WAL_HISTORY_BUCKET_NAME` | Optional | unset | Bucket name. Blank means unset. | Bucket, reached through the configured object store, that is read when replaying webhook payload history archived to S3. Set `WAL_HISTORY_STORAGE=pg` or set this explicitly. | [config.rs] |
| `HOSTNAME` | Optional | normally set by the OS or container runtime | String | `development` profile only: object-key prefix for WAL-history archive reads. Ignored in other profiles. | [config.rs] |

### Object storage (S3-compatible)

The server talks to any S3-compatible service (Cloudflare R2, AWS S3 and others) using static credentials. No provider endpoint is inferred.

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `S3_ENDPOINT` | **Yes, for `serve`** | unset | Absolute `http(s)` URL. A trailing `/` is removed. | S3 API endpoint for all object operations. `serve` fails with `S3_ENDPOINT is required; configure an S3-compatible object store` if it is missing. | [config.rs], [s3_storage.rs] |
| `S3_PUBLIC_ENDPOINT` | Optional | the value of `S3_ENDPOINT` | Absolute `http(s)` URL | Origin used in browser-facing presigned download URLs. Set it when browsers reach storage through a different host than the server does, and prefer a dedicated files domain (for example the R2 custom domain `files.<domain>`) so uploaded HTML, SVG and JavaScript can never share an origin with the dashboard or API. The lifetime is set by `INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS`. | [config.rs], [s3_storage.rs] |
| `INSTANT_STORAGE_MAX_UPLOAD_BYTES` | Optional | `104857600` (100 MiB) | Positive integer; blank means unset | Largest object accepted by a single upload. Declared `Content-Length` values over the cap are rejected with `413` before any bytes are stored, and chunked bodies are aborted as soon as the counted bytes pass it. Applies to `/storage/upload`, the signed-upload consume route, the dashboard upload and `/admin/storage/upload`. | [config.rs], [admin_storage.rs] |
| `INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS` | Optional | `604800` (7 days) | Integer, clamped to `60`–`604800`; blank means unset | Lifetime of presigned download URLs. Values below one minute become `60`, and values above S3's seven-day limit become `604800`. | [config.rs], [s3_storage.rs] |
| `INSTANT_S3_REGION` | **Yes, when `S3_ENDPOINT` is set** | `us-east-1` | Region string (use `auto` for R2). Blank means unset. | SigV4 signing region. `AWS_REGION` is **not** consulted. | [config.rs] |
| `S3_VIRTUAL_HOST_STYLE` | Optional | `false` | `true` or `false` | `true` uses virtual-hosted-style addressing (`<bucket>.<endpoint>`). `false` uses path-style addressing (`<endpoint>/<bucket>`). | [config.rs], [s3_storage.rs] |
| `S3_BUCKET` | **Yes, when `S3_ENDPOINT` is set** | unset; startup fails with `S3_BUCKET is required when S3_ENDPOINT is configured` | Bucket name | Bucket for app file storage. | [config.rs] |
| `S3_APP_BACKUPS_BUCKET` | **Yes, in practice** | unset; `serve` fails with `S3_APP_BACKUPS_BUCKET is required for production routes` | Bucket name | Bucket for app backups. Uses the same endpoint and credentials as `S3_BUCKET` but is never the same bucket as file storage. | [config.rs], [production_routes.rs] |
| `INSTANT_NIGHTLY_APP_BACKUPS` | Optional | unset (nightly backups run only in the `production` profile) | Exactly `true` or `false`; blank is treated as unset; anything else fails startup | Overrides the profile check for the nightly per-app backup worker. `true` enables it in any profile and `false` disables it even in production. Leave unset to keep the profile default. | [config.rs], [production_routes.rs] |
| `INSTANT_S3_ACCESS_KEY_ID` | **Yes, for `serve`** | unset | String | Static access key for S3 API requests. Must be set together with `INSTANT_S3_SECRET_ACCESS_KEY`. | [s3_storage.rs] |
| `INSTANT_S3_SECRET_ACCESS_KEY` | **Yes, for `serve`** | unset | String | Secret key that pairs with `INSTANT_S3_ACCESS_KEY_ID`. | [s3_storage.rs] |
| `INSTANT_S3_SESSION_TOKEN` | Optional | unset | String | Session token for temporary S3 credentials. | [s3_storage.rs] |
| `INSTANT_S3_PRESIGN_ACCESS_KEY_ID` | Optional | unset (download URLs are signed with the request credentials) | String | Separate credentials used **only** to sign download URLs. Must be set together with `INSTANT_S3_PRESIGN_SECRET_ACCESS_KEY`. | [production_routes.rs], [s3_storage.rs] |
| `INSTANT_S3_PRESIGN_SECRET_ACCESS_KEY` | Optional | unset | String | Secret that pairs with `INSTANT_S3_PRESIGN_ACCESS_KEY_ID`. | [production_routes.rs] |
| `INSTANT_S3_PRESIGN_SESSION_TOKEN` | Optional | unset | String | Session token for the presigning credentials. | [production_routes.rs] |

### Email

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_EMAIL_PROVIDER` | Optional | auto-detect (see below) | `postmark`, `sendgrid`, `resend` or `cloudflare`. Case-insensitive. Blank means auto. | Selects the transactional email provider. | [config.rs], [email_router.rs] |
| `POSTMARK_TOKEN` | Optional | unset | String | Postmark server token. | [config.rs] |
| `POSTMARK_ACCOUNT_TOKEN` | Optional | unset | String | Postmark account token, used to create and inspect custom app sender signatures. | [config.rs] |
| `SENDGRID_TOKEN` | Optional | unset | String | SendGrid API token. | [config.rs] |
| `RESEND_TOKEN` | Optional | unset | String | Resend API token. | [config.rs] |
| `CLOUDFLARE_ACCOUNT_ID` | Required with `cloudflare` | unset | String | Cloudflare account for Email Sending. | [config.rs] |
| `CLOUDFLARE_EMAIL_API_TOKEN` | Required with `cloudflare` | unset | String | Cloudflare API token. It needs Email Sending Edit to send, plus Zone Read and Email Sending Read on every zone whose domains apps use for custom senders. | [config.rs], [cloudflare_email.rs] |
| `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL` | Required with `cloudflare` | unset | Email address | Fallback sender for app, dashboard and team email when their own `*_SENDER_EMAIL` is unset. The fallback applies with every provider. | [config.rs] |
| `INSTANT_APP_EMAIL_SENDER_EMAIL` | Optional; set it | `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL`, otherwise none | Email address | Platform sender for application login (magic-code) email. With `cloudflare`, an app's own template sender is used only while that app's sender domain is verified; otherwise app email uses this address. When unset, app login codes are logged at WARN level instead of sent. | [config.rs], [email_router.rs], [postmark.rs] |
| `INSTANT_APP_EMAIL_SENDER_NAME` | Optional | unset | String | Display name for the app sender. | [config.rs] |
| `INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL` | Optional; set it | `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL`, otherwise none | Email address | Sender for dashboard login codes. When unset, dashboard login codes are logged at WARN level instead of sent. | [config.rs], [postmark.rs] |
| `INSTANT_DASHBOARD_EMAIL_SENDER_NAME` | Optional | `Instant` | String | Display name for the dashboard sender. | [config.rs] |
| `INSTANT_TEAM_EMAIL_SENDER_EMAIL` | Optional; set it | `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL`, otherwise none | Email address | Sender for team email. When unset, dashboard invite and team email operations return an error that names this variable. | [config.rs], [postmark.rs] |
| `INSTANT_TEAM_EMAIL_SENDER_NAME` | Optional | `Instant` | String | Display name for the team sender. | [config.rs] |
| `INSTANT_MAGIC_CODE_DAILY_LIMIT_PER_APP` | Optional | `1000` | Positive integer | Deployment default for the per-app daily magic-code send quota. The `magic-code-daily-limit-per-app` config-app flag overrides it. Exceeding the quota returns the same rate-limited error as the per-email limit. | [magic_code_rate_limit.rs] |
| `INSTANT_MAGIC_CODE_RATE_LIMIT_PER_IP_HOUR` | Optional | `30` | Positive integer | Deployment default for the public runtime route's per-client-IP hourly magic-code quota. The `magic-code-rate-limit-per-ip-hour` config-app flag overrides it. The address is the client IP resolved through `INSTANT_TRUSTED_PROXY_CIDRS`; requests from private addresses are not IP-limited; behind Caddy or Cloudflare set `INSTANT_TRUSTED_PROXY_CIDRS` to the proxy network. | [magic_code_rate_limit.rs], [runtime_auth.rs] |
| `INSTANT_MAGIC_CODE_VERIFY_LIMIT_PER_IP_HOUR` | Optional | `60` | Positive integer | Deployment default for the public runtime verify route's per-client-IP hourly quota. The `magic-code-verify-limit-per-ip-hour` config-app flag overrides it. It uses the same resolved client address and private-address exemption as the send quota. | [magic_code_rate_limit.rs], [runtime_auth.rs] |

How the provider is chosen ([email_router.rs]):

- `cloudflare`: always enabled. `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_EMAIL_API_TOKEN` and `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL` must all be set, or startup fails.
- `postmark`, `sendgrid`, `resend`: use that provider. Delivery is disabled if its token is unset.
- Unset (auto):
  - Only `RESEND_TOKEN` set: Resend.
  - `SENDGRID_TOKEN` set without `POSTMARK_TOKEN`: SendGrid.
  - `POSTMARK_TOKEN` set: Postmark.
  - No token set: delivery is disabled.
- **When delivery is disabled, app and dashboard login codes are written to the log at WARN level, together with the recipient address.** Do not run a public deployment this way.

#### Per-app sender domains

With `INSTANT_EMAIL_PROVIDER=cloudflare`, each app may set its own sender email and display name in the dashboard. The default remains the platform sender from `INSTANT_APP_EMAIL_SENDER_EMAIL`.

- An app's custom sender is used only while its domain is verified. Verification requires both: the domain is enabled for **Email Service → Email Sending** in the Cloudflare account, and the app's dashboard Admin or Owner confirms ownership with the code sent to the sender address.
- The server checks Cloudflare's zone-scoped Email Sending subdomain list. Unverified, disabled or removed domains fall back to the platform sender, and the dashboard shows the domain as unverified together with the last error.
- Verified domains are re-checked daily. A send that Cloudflare rejects as unverified is retried once from the platform sender and marks the domain unverified until it is verified again.
- Changing a sender address invalidates its ownership confirmation, so the new address must be verified before it is used.
- The existing per-app daily magic-code quota applies to all of these sends.

The Postmark provider keeps its sender-signature flow; Cloudflare domain verification does not apply to it.

### Dashboard access, cookies and OAuth

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_SECURE_COOKIES` | Optional | off | Exactly `true` turns it on | Matters only in the `development` profile. Sets the `Secure` attribute on OAuth and session cookies when `INSTANT_BACKEND_URL` is `https`. In other profiles this happens automatically for `https`, and a plain `http` backend URL never gets secure cookies. | [config.rs] |
| `INSTANT_SUPERUSER_EMAIL` | Optional | unset | Email address. Trimmed and lowercased. | Dashboard account that is always authorized as a deployment operator and can manage the entire deployment. When set, rate-limit errors name this address as the deployment administrator; otherwise they point to the operator without an email address. Restart the server after you change it. | [config.rs], [production_routes.rs], [app_rate_limit.rs] |
| `INSTANT_DASHBOARD_ALLOWED_EMAILS` | Optional | unset (no restriction) | Comma-separated email addresses; blank is treated as unset. Each entry is trimmed, lowercased and de-duplicated. Every entry needs a non-empty local part and a domain that contains a `.`; empty entries, such as from a trailing comma, are rejected. | Restricts which emails can sign in to or sign up for the dashboard, including Google logins and existing dashboard bearer tokens. Listed emails may sign up even when signup is closed. Authentication of application end users is not affected. | [config.rs] |
| `INSTANT_DASHBOARD_SIGNUP` | Optional | `closed` | `open` or `closed` | Controls signup for emails outside the allowlist and the superuser. `closed` is the default and rejects new dashboard accounts. `open` accepts any email; the server logs a warning at startup. A config-app `dashboard-signups` flag takes precedence. | [config.rs], [postgres_dashboard_policy.rs], [postgres_dashboard_identity.rs] |
| `INSTANT_EPHEMERAL_APPS` | Optional | `false` | Exactly `true` enables it; anything else is off | Enables unauthenticated `POST /dash/apps/ephemeral` creation. A config-app `ephemeral-apps-enabled` flag takes precedence. Leave off unless you operate the upstream ephemeral-app flow. | [config.rs], [production_dashboard.rs], [postgres_dashboard_policy.rs] |
| `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID` | Optional; set together with the secret | unset | String | Enables "Sign in with Google" on the dashboard. Register `<INSTANT_BACKEND_URL>/dash/oauth/callback` as the authorized redirect URI. | [config.rs] |
| `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET` | Optional; set together with the ID | unset | String | Client secret for the Google dashboard login. | [config.rs] |
| `INSTANT_SHARED_OAUTH_CLIENTS` | Optional | unset | JSON array of `{"provider_name": "...", "client_id": "...", "client_secret": "..."}`. All fields are non-blank after trimming, and `provider_name` values must be unique. | Operator-provided OAuth credentials, keyed by provider name. App OAuth clients that opt into shared credentials use them, and the dashboard lists those providers. | [config.rs] |
| `INSTANT_DASHBOARD_PRIVATE` | Optional | `false` | `true` or `false`; blank is treated as unset | `true` makes `INSTANT_DASHBOARD_GATEWAY_TOKEN` mandatory. | [production_routes.rs] |
| `INSTANT_DASHBOARD_GATEWAY_TOKEN` | Required when `INSTANT_DASHBOARD_PRIVATE=true` | unset | 32–128 characters from `A–Z a–z 0–9 - _`; blank is treated as unset | When set, dashboard management routes accept only requests that carry exactly one matching `x-instant-dashboard-gateway-token` header; other requests get 403. The header is removed before the request reaches handlers. Runtime and Admin SDK routes are not affected. | [production_routes.rs], [dashboard_gateway.rs] |
| `INSTANT_SESSION_COUNTER_TOKEN` | Optional | unset | Non-empty string | Enables the `/dash/session_counts` WebSocket, which returns 503 while this is unset. A client authenticates by sending `{"token":"<value>"}` and then receives session reports every 5 seconds. | [instant-http dashboard.rs] |

#### App OAuth ID-token audiences

Every app OAuth client only accepts ID tokens whose `aud` claim contains the client's resolved `client_id` or one of its `extra_audiences`. A client without a secret is not exempt. Register native identifiers, such as iOS bundle IDs or Android client IDs, as `extra_audiences` on the OAuth client (at most 20 entries, each at most 256 characters).

Clerk session tokens do not reliably include `aud`. Configure the Clerk OAuth client with a `client_id` and make Clerk issue an `aud` claim equal to that value (for example through a JWT template or custom session-token claim); tokens without a matching audience are rejected. The same value can be placed in `extra_audiences` when the Clerk client's `client_id` differs. Clerk also sets `azp` to the requesting origin, and every present `azp` must be accepted, so add each application origin to `extra_audiences` as well.

Provider notes for `signInWithIdToken` ([runtime_oauth.rs]):

- **Clerk.** See the audience and origin requirements above. The dashboard and CLI create a Clerk client without a `client_id`; set it through the app OAuth-client API before signing in.
- **LinkedIn.** LinkedIn ID tokens use the OAuth client ID as `aud` and `https://www.linkedin.com/oauth` as `iss`, so a LinkedIn client with its client ID configured signs in without extra claims. The authorization-code flow decodes the ID token returned by the token endpoint, or reads the userinfo endpoint when there is none.
- **Firebase.** Firebase only supports `signInWithIdToken`; it has no authorization-code flow. Its ID tokens use the Firebase project ID as `aud` and `https://securetoken.google.com/<project>` as `iss`. The dashboard and CLI create a Firebase client without a client ID, and an ID-token sign-in is rejected before the audience check when the client has none. Set the client's `client_id` to the project ID (`PATCH /dash/apps/{app_id}/oauth_clients/{id}`) after creating it.

### Proxies

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_TRUSTED_PROXY_CIDRS` | Optional | unset (no proxy trusted) | Comma-separated IPv4 or IPv6 CIDRs, for example `10.0.0.0/8, fd00::/7`. A prefix length is required. Blank means unset. | Client IP resolution for dashboard requests, the public runtime magic-code per-IP quota, and `request.ip` permission rules. If the socket peer is in a trusted network, `X-Forwarded-For` is walked from right to left through trusted hops. `X-Real-IP` is used only when there is no `X-Forwarded-For`. Both headers are removed before dashboard handlers run. The per-IP quota uses the client IP resolved through `INSTANT_TRUSTED_PROXY_CIDRS`; requests from private addresses are not IP-limited; behind Caddy or Cloudflare set `INSTANT_TRUSTED_PROXY_CIDRS` to the proxy network. The Docker entrypoint also accepts the special value `auto`, which resolves `INSTANT_TRUSTED_PROXY_HOST` to host routes (see [Container entrypoint variables](#container-entrypoint-variables)); the binary itself rejects `auto`. | [config.rs], [dashboard.rs] |

### Request limits and timeouts

Management and admin request bodies are capped at 1 MiB before authentication: `/dash/*` (except the restore and file-upload routes), `/admin/*` (except `/admin/storage/upload`) and `/superadmin/*`. The dashboard file upload accepts 64 MiB and the restore upload accepts 1 GiB. Oversized requests get `413 Payload Too Large` with the standard error body. Non-streaming routes have a 30 second overall deadline; WebSocket upgrades, SSE routes and storage uploads/downloads are excluded.

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_MAX_LIVE_SESSIONS_PER_IP` | Optional | `100` | Positive integer | Maximum concurrent runtime WebSocket and SSE sessions from one public client IP. The address is resolved through `INSTANT_TRUSTED_PROXY_CIDRS`; private, loopback and link-local addresses are exempt. The config-app `max-live-sessions-per-ip` flag overrides it at startup. | [runtime_realtime.rs] |
| `INSTANT_MAX_SESSIONS_PER_APP` | Optional | `10000` | Positive integer | Maximum concurrent runtime WebSocket and SSE sessions attached to one app, so a runaway client cannot exhaust process memory or file descriptors for a whole tenant. A new session over the cap gets a `rate-limited` error and the connection must retry later. The config-app `max-sessions-per-app` flag overrides it at startup. | [runtime_realtime.rs] |
| `INSTANT_REFRESH_WAITERS_PER_APP` | Optional | `64` | Positive integer | Maximum queued realtime refreshes for one app. Excess waiters fail fast with the existing `rate-limited` refresh-admission error instead of extending the FIFO queue for other apps. | [runtime_realtime.rs] |
| `INSTANT_MAX_SUBSCRIPTIONS_PER_SESSION` | Optional | `500` | Positive integer | Maximum distinct InstaQL queries a runtime session may keep subscribed at once. The SDK de-duplicates queries by content before sending `add-query`, so this counts distinct live queries, not `useQuery` callers. The config-app `max-subscriptions-per-session` flag overrides it at startup. | [runtime_realtime.rs] |
| `INSTANT_MAX_INSTAQL_QUERY_BYTES` | Optional | `262144` (256 KiB) | Positive integer | Maximum serialized JSON size of one InstaQL query in a runtime `add-query` message. The config-app `max-instaql-query-bytes` flag overrides it at startup. | [runtime_realtime.rs] |
| `INSTANT_RESULT_CACHE_MAX_BYTES` | Optional | a quarter of the container memory limit, at most 4 GiB | Positive integer | Process-wide cap on the ready bytes retained by every per-app session result cache. Each app still honors its own `datalog-cache-max-weight`; when the sum exceeds this cap, the least recently used entries are evicted across apps. The default reads the cgroup v2 `memory.max`, then `/proc/meminfo`, and falls back to 1 GiB when neither is readable. The production compose files give the server `mem_limit: ${INSTANT_SERVER_MEM_LIMIT:-8g}`, so the default budget there is 2 GiB. | [runtime_realtime/query_cache.rs], [process_memory.rs] |

Config-app flags are read once when the process starts; restart the server after changing them. Malformed or non-positive values are ignored and the environment default stays in effect.

### Shutdown drain

On SIGINT or SIGTERM the server first withdraws `/health/system`, then moves live clients off the process: new WebSocket upgrades and SSE sessions get `503` with `Retry-After: 1`, existing WebSockets receive close code `1012` (service restart), and SSE streams end cleanly. Only after that drain does the normal shutdown grace start and, if tasks are still running, the process aborts them. The compose files set `stop_grace_period: 45s` so Docker does not send SIGKILL before the drain finishes.

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_DRAIN_BUDGET_SECONDS` | Optional | `30` | Positive integer | Upper bound on the live-session drain; a 10 s shutdown grace follows it. Keep the sum below the container stop timeout (compose sets `stop_grace_period: 45s`). Read when termination begins, so a restart is not required to change it. | [production_process.rs] |

### Observability

The server writes JSON logs to stdout. When `SENTRY_DSN` is set, it also reports panics and error-level log events to any Sentry-protocol server, such as a self-hosted Bugsink instance. Reporting sends no request bodies, cookies, headers, query strings or personal data, and it does not trace requests or record sessions. Older exporter variables have no effect ([telemetry.rs], [error_reporting.rs]).

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `RUST_LOG` | Optional | `info,pg_walstream=warn` | `tracing-subscriber` `EnvFilter` directives | Log level and filtering. The default keeps the replication client's per-second status lines out of the logs. | [telemetry.rs] |
| `SENTRY_DSN` | Optional | unset (error reporting off) | Sentry-protocol DSN, for example `http://<key>@127.0.0.1:8000/1`. Blank means unset. | Enables error reporting. Panics and `error!` events are sent as events; `warn!` events are sent as breadcrumbs. Set this to the Bugsink project DSN (see [operations.md](operations.md#error-tracking)). An invalid DSN fails startup. | [config.rs], [error_reporting.rs] |
| `SENTRY_ENVIRONMENT` | Optional | `production` | String. Blank means unset. | Environment label attached to every reported event. | [config.rs], [error_reporting.rs] |
| `INSTANT_METRICS_ADDR` | Optional | unset (metrics off) | Loopback socket address with a nonzero port, for example `127.0.0.1:9464`; with `INSTANT_METRICS_ALLOW_NONLOOPBACK=true`, a private, IPv6 unique-local, or unspecified address is also accepted | Serves Prometheus text metrics at `GET /metrics` and turns on per-request latency counters. Serves one scrape at a time; concurrent scrapes get 503. A bind failure fails startup. Loopback is the default so only a process on the same host or network namespace can scrape; the opt-in exists for a private container network. | [main.rs], [metrics.rs] |
| `INSTANT_METRICS_ALLOW_NONLOOPBACK` | Optional | `false` | `true` or `false` | Allows `INSTANT_METRICS_ADDR` to bind a private RFC1918, IPv6 unique-local, or unspecified address. Public addresses are still rejected. Use it only when another container must scrape the metrics endpoint over a private network. | [metrics.rs] |

### Integrations

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `INSTANT_ON_INSTANT_APP_ID` | Optional | unset | UUID | Upstream-internal setting: realtime invalidation for this app reads from the primary database (`public` schema) instead of a BYOP connection string. Leave unset. | [config.rs], [production_routes.rs] |

### Runtime and library variables

| Variable | Required | Default | Format | What it controls | Source |
| --- | --- | --- | --- | --- | --- |
| `TOKIO_WORKER_THREADS` | Optional | number of CPU cores | Positive integer | Number of async worker threads in the Tokio runtime. The binary does not set a thread count itself, so Tokio reads this variable. Each worker thread has a fixed 16 MiB stack. | [main.rs] |

### Container entrypoint variables

The Docker image's entrypoint, [docker-entrypoint.sh], reads these variables. The binary does not.

| Variable | Required | Default | What it controls |
| --- | --- | --- | --- |
| `INSTANT_CONFIG_DIR` | Optional | `/home/instant/config` | For `serve`, `configure-byop` and `migrate`: if any of the three [required secrets](#required-secrets) is unset, the entrypoint sources `<dir>/secrets.env`. If that file does not exist yet, it first creates it with `generate-secrets` (mode `600`). Mount a persistent volume here. Every instance must use the same secrets, so do not let instances generate separate files. |
| `INSTANT_SERVER_BIN` | Optional | `/usr/local/bin/instant-server` | Path of the server binary the entrypoint runs. |
| `INSTANT_TRUSTED_PROXY_CIDRS=auto` | Optional | — | The entrypoint resolves `INSTANT_TRUSTED_PROXY_HOST` (default `caddy`) with `getent ahosts` and replaces `auto` with the resolved addresses as `/32` or `/128` routes. If the name does not resolve, it starts with no trusted proxies and prints a warning. |
| `INSTANT_TRUSTED_PROXY_HOST` | Optional | `caddy` | Proxy service name resolved by `INSTANT_TRUSTED_PROXY_CIDRS=auto`. |

### Test-only variables

`TEST_DATABASE_URL`, `TEST_DATABASE_STATEMENT_COUNTS` and the `INSTANT_LIVE_S3_*` variables are read only by the test suites. The server binary ignores them. See [`server-rs/run-postgres-acceptance.sh`](../server-rs/run-postgres-acceptance.sh) for how live database tests are run.

## Validation rules

The binary fails closed: it refuses to start when configuration is missing or malformed instead of falling back to a default.

**When configuration loads** (every command except `--help`, `--version`, `generate-secrets` and `healthcheck`; [config.rs]):

| Condition | Error |
| --- | --- |
| `PORT`, `CONNECTION_POOL_SIZE` or `INSTANT_SKETCH_BATCH_*` is not an integer of the right range (a port must be 0–65535) | `<NAME> must be an integer` |
| `CONNECTION_POOL_SIZE` is `0` | `CONNECTION_POOL_SIZE must be greater than zero` |
| A sketch batch value is outside 1–65536 transactions, 1–67108864 bytes or 1–10000 ms | `sketch batching requires 1..65536 transactions, 1..67108864 WAL bytes and 1..10000 milliseconds` |
| `DATABASE_URL` or `NEXT_DATABASE_URL` is not a `postgres://` or `postgresql://` URL that `tokio-postgres` can parse | `DATABASE_URL must be a PostgreSQL connection URL` (the message names `DATABASE_URL` in both cases) |
| `INSTANT_BACKEND_URL`, `INSTANT_DASHBOARD_URL`, `S3_ENDPOINT` or `S3_PUBLIC_ENDPOINT` is not an absolute `http(s)` URL with a host | `<NAME> must be an absolute HTTP(S) URL` |
| `PRODUCTION=true` or `STAGING=true` without `INSTANT_BACKEND_URL` | `INSTANT_BACKEND_URL is required in the production and staging profiles` |
| `PRODUCTION=true` or `STAGING=true` without `INSTANT_DASHBOARD_URL` | `INSTANT_DASHBOARD_URL is required in the production and staging profiles` |
| `S3_ENDPOINT` is set without `INSTANT_S3_REGION` | `INSTANT_S3_REGION is required when S3_ENDPOINT is configured` |
| `S3_ENDPOINT` is set without `S3_BUCKET` | `S3_BUCKET is required when S3_ENDPOINT is configured` |
| `S3_VIRTUAL_HOST_STYLE` is not `true` or `false` | `S3_VIRTUAL_HOST_STYLE must be true or false` |
| `INSTANT_STORAGE_MAX_UPLOAD_BYTES` is `0` (blank is unset) | `INSTANT_STORAGE_MAX_UPLOAD_BYTES must be greater than zero` |
| An entry in `INSTANT_TRUSTED_PROXY_CIDRS` is not a valid CIDR | `INSTANT_TRUSTED_PROXY_CIDRS contains an invalid CIDR: <entry>` |
| Only one of the Google dashboard OAuth ID and secret is set | `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID and INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET must be set together` |
| `INSTANT_DASHBOARD_ALLOWED_EMAILS` is set and contains an invalid or empty entry (blank is unset) | `INSTANT_DASHBOARD_ALLOWED_EMAILS must be a non-empty comma-separated list of email addresses` |
| `INSTANT_DASHBOARD_SIGNUP` is neither `open` nor `closed` (blank is unset) | `INSTANT_DASHBOARD_SIGNUP must be open or closed` |
| `INSTANT_NIGHTLY_APP_BACKUPS` is set to anything other than `true`, `false` or blank | `INSTANT_NIGHTLY_APP_BACKUPS must be true or false` |
| `INSTANT_SHARED_OAUTH_CLIENTS` is not valid JSON of the required shape, has blank fields or repeats a provider | `INSTANT_SHARED_OAUTH_CLIENTS must be a JSON array of unique, non-blank ...` |
| `INSTANT_ON_INSTANT_APP_ID` is not a UUID | `INSTANT_ON_INSTANT_APP_ID must be a UUID` |
| `INSTANT_EMAIL_PROVIDER` has an unknown value | `INSTANT_EMAIL_PROVIDER must be postmark, sendgrid, or cloudflare` (`resend` is also accepted, although the message omits it) |
| `INSTANT_EMAIL_PROVIDER=cloudflare` without all three Cloudflare settings | `Cloudflare email requires CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_EMAIL_API_TOKEN, and INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL` |
| `DATABASE_CLUSTER_ID` or `NEXT_DATABASE_CLUSTER_ID` is set | `<NAME> is not supported by the self-hosted instant-server build` |
| `SENTRY_DSN` is set but is not a Sentry-protocol DSN (blank is unset) | `SENTRY_DSN is not a valid Sentry DSN` |

**When secrets load** (`serve` and `configure-byop`; [production_routes.rs]):

| Condition | Error |
| --- | --- |
| `INSTANT_AEAD_KEYSET_JSON`, `INSTANT_WEBHOOK_KEYSET_JSON` or `INSTANT_RATE_LIMIT_HMAC_KEY` is missing or blank | `<NAME> is required` |
| A `*_KMS_KEY_URL` variable is set | `<NAME> is not supported by the self-hosted instant-server build` |
| Only one of `INSTANT_S3_PRESIGN_ACCESS_KEY_ID` and `INSTANT_S3_PRESIGN_SECRET_ACCESS_KEY` is set | `... must be set together` |
| `INSTANT_DASHBOARD_PRIVATE` is neither `true` nor `false` (blank is unset) | `INSTANT_DASHBOARD_PRIVATE must be true or false` |
| `INSTANT_DASHBOARD_PRIVATE=true` without a gateway token | `INSTANT_DASHBOARD_GATEWAY_TOKEN is required` |
| The gateway token is not 32–128 header-safe characters | `INSTANT_DASHBOARD_GATEWAY_TOKEN must contain 32–128 header-safe alphanumeric, hyphen, or underscore characters` |

**During `serve` startup, before the listener binds:**

- Every database pool opens one connection and runs `SELECT 1`. Migrations and the system catalog must be current (see [Migrations are never applied by `serve`](#migrations-are-never-applied-by-serve)).
- `S3_ENDPOINT` must be set, and `INSTANT_S3_ACCESS_KEY_ID` and `INSTANT_S3_SECRET_ACCESS_KEY` must be set together ([s3_storage.rs]).
- A backup bucket must resolve. `S3_APP_BACKUPS_BUCKET` has no default in any profile.
- `INSTANT_METRICS_ADDR`, if set, must be a loopback address with a nonzero port that can be bound, or a private/unspecified address with `INSTANT_METRICS_ALLOW_NONLOOPBACK=true` ([metrics.rs]).

## Connection pool sizing

`CONNECTION_POOL_SIZE` is the total size of the main database pool. When it is 8 or more, the pool is split into three independent sub-pools ([`workload_capacities` in postgres_pool.rs][postgres_pool.rs]):

```text
control     = min(total / 4, total - 7)              (integer division)
refresh     = min((total - control) / 2, total - control - 6)
interactive = total - control - refresh
```

- **interactive**: queries and transactions issued by clients. It always gets at least 6 connections.
- **refresh**: re-runs subscribed realtime queries after invalidation.
- **control**: fresh per-operation reads of policy, permission and app-status data.

| `CONNECTION_POOL_SIZE` | control | refresh | interactive |
| --- | --- | --- | --- |
| 8 (minimum for partitioning) | 1 | 1 | 6 |
| 16 | 4 | 6 | 6 |
| 20 (non-hosted default) | 5 | 7 | 8 |
| 48 | 12 | 18 | 18 |
| 400 (hosted default) | 100 | 150 | 150 |

Below 8, the pool is not partitioned, and all workloads share one pool of that size. Pool wait, connect and recycle timeouts are fixed at 30 s, 10 s and 10 s.

### Per-connection statement bounds

Every pooled connection gets its session bounds once, when it is created. A per-query `SET LOCAL statement_timeout` still overrides the session value for that transaction. Pools used for backups, restores, snapshot exports and bulk maintenance (`backup_control`, `backup_snapshot`, `restore_worker`, `custodian`, `nightly_backup_control`, `partition_retention`, `sketch_aggregator_*`, `*_sweeper`, `triples_size_collector`) keep PostgreSQL's own statement and lock timeouts so a long stream, a restore DDL wait or the fleet-wide sketch bootstrap lock is not cancelled halfway; they still get the idle-in-transaction bound.

| Variable | Default | Format | What it controls |
| --- | --- | --- | --- |
| `INSTANT_PG_STATEMENT_TIMEOUT_INTERACTIVE_MS` | `15000` (15 s) | Non-negative integer milliseconds; blank keeps the default | `statement_timeout` on the interactive pool and on unpartitioned (`shared`) pools. |
| `INSTANT_PG_STATEMENT_TIMEOUT_BACKGROUND_MS` | `30000` (30 s) | Same | `statement_timeout` on refresh, control and other bounded background pools. |
| `INSTANT_PG_LOCK_TIMEOUT_MS` | `5000` (5 s) | Same | `lock_timeout` on interactive and background pools, so a queued row or advisory lock fails instead of pinning a connection. Maintenance pools keep the server default. |
| `INSTANT_PG_IDLE_IN_TX_TIMEOUT_MS` | `60000` (60 s) | Same | `idle_in_transaction_session_timeout` on every pool, so an abandoned transaction releases its locks and snapshot. |

The interactive pool also enforces a per-app share of `max(4, interactive / 3)` concurrent database leases per app. An app that stays over its share gets `rate-limited` (`Realtime query capacity is temporarily exhausted`) instead of delaying every other tenant; unused per-app semaphores are dropped as soon as no query holds or waits on them. Durable rate limits use a three-connection pool so one slow bucket transaction does not serialize other apps.

Sizing guidance:

- The load tests used `CONNECTION_POOL_SIZE=48`. A small database, for example one with 2 GiB of RAM, works with `16`.
- The main pool is not the only source of connections. Each `serve` process also keeps about 34 dedicated small pools for background workers and subsystems (35 with `PRODUCTION=true`), including a three-connection pool for durable rate limits, plus logical-replication streaming connections and one control connection per BYOP app. The WAL durable sink (`wal_durable_sink`) has its own single connection so presence and broadcast writes cannot delay WAL history, webhook and cluster publication. When `NEXT_DATABASE_URL` points at another database, the process opens up to `CONNECTION_POOL_SIZE` more connections there. Set PostgreSQL `max_connections` to at least `instances × (CONNECTION_POOL_SIZE + 41)`, with headroom for migrations and administration.

## Minimal production configuration

This example is a minimal self-hosted setup. Like the repository's Compose files, it leaves `PRODUCTION`, `STAGING` and `TEST` unset, so it runs with the `development` profile and sets every profile-dependent value explicitly. All values are placeholders.

If you do set `PRODUCTION=true` or `STAGING=true`, startup additionally requires explicit `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL`; `S3_BUCKET` is required whenever `S3_ENDPOINT` is set. `S3_APP_BACKUPS_BUCKET` is required for the production routes, and any email you want delivered needs its `INSTANT_*_EMAIL_SENDER_EMAIL` (or the shared `INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL`). Set `WAL_HISTORY_BUCKET_NAME` if you do not disable the WAL-history archive with `WAL_HISTORY_STORAGE=pg`.

Run `instant-server migrate up` with this environment before the first `serve`, and again before each upgrade.

```sh
# Public URLs (TLS terminated by a reverse proxy)
INSTANT_BACKEND_URL=https://api.example.com
INSTANT_DASHBOARD_URL=https://dashboard.example.com
INSTANT_SECURE_COOKIES=true
INSTANT_TRUSTED_PROXY_CIDRS=10.0.0.0/8

# Database (wal_level=logical, wal2json installed)
DATABASE_URL=postgresql://instant:<password>@db.example.com:5432/instant?sslmode=require
CONNECTION_POOL_SIZE=16
WAL_HISTORY_STORAGE=pg

# Secrets: generate once with `instant-server generate-secrets`; identical on every instance
INSTANT_AEAD_KEYSET_JSON='<generated>'
INSTANT_WEBHOOK_KEYSET_JSON='<generated>'
INSTANT_RATE_LIMIT_HMAC_KEY='<generated>'

# S3-compatible object storage
S3_ENDPOINT=https://storage.example.com
S3_PUBLIC_ENDPOINT=https://storage.example.com
INSTANT_S3_REGION=auto
S3_BUCKET=example-files
S3_APP_BACKUPS_BUCKET=example-backups
INSTANT_S3_ACCESS_KEY_ID=<access-key-id>
INSTANT_S3_SECRET_ACCESS_KEY=<secret-access-key>

# Email (or postmark / sendgrid / resend with the matching token)
INSTANT_EMAIL_PROVIDER=cloudflare
CLOUDFLARE_ACCOUNT_ID=<cloudflare-account-id>
CLOUDFLARE_EMAIL_API_TOKEN=<email-sending-token>
INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL=no-reply@example.com

# Dashboard access
INSTANT_SUPERUSER_EMAIL=admin@example.com
INSTANT_DASHBOARD_ALLOWED_EMAILS=admin@example.com
```

Point the SDK at the server with `apiURI: "https://api.example.com"` and `websocketURI: "wss://api.example.com/runtime/session"`.

[main.rs]: ../server-rs/crates/instant-server/src/main.rs
[config.rs]: ../server-rs/crates/instant-server/src/config.rs
[production_routes.rs]: ../server-rs/crates/instant-server/src/production_routes.rs
[production_process.rs]: ../server-rs/crates/instant-server/src/production_process.rs
[production_platform.rs]: ../server-rs/crates/instant-server/src/production_platform.rs
[postgres_pool.rs]: ../server-rs/crates/instant-server/src/postgres_pool.rs
[partition_retention.rs]: ../server-rs/crates/instant-server/src/partition_retention.rs
[custodian.rs]: ../server-rs/crates/instant-server/src/custodian.rs
[migrations.rs]: ../server-rs/crates/instant-server/src/migrations.rs
[replication.rs]: ../server-rs/crates/instant-server/src/replication.rs
[s3_storage.rs]: ../server-rs/crates/instant-server/src/s3_storage.rs
[email_router.rs]: ../server-rs/crates/instant-server/src/email_router.rs
[cloudflare_email.rs]: ../server-rs/crates/instant-server/src/cloudflare_email.rs
[dashboard.rs]: ../server-rs/crates/instant-server/src/dashboard.rs
[runtime_oauth.rs]: ../server-rs/crates/instant-server/src/runtime_oauth.rs
[instant-http dashboard.rs]: ../server-rs/crates/instant-http/src/dashboard.rs
[dashboard_gateway.rs]: ../server-rs/crates/instant-http/src/dashboard_gateway.rs
[telemetry.rs]: ../server-rs/crates/instant-server/src/telemetry.rs
[error_reporting.rs]: ../server-rs/crates/instant-server/src/error_reporting.rs
[metrics.rs]: ../server-rs/crates/instant-server/src/metrics.rs
[process_memory.rs]: ../server-rs/crates/instant-server/src/process_memory.rs
[self_hosted_secrets.rs]: ../server-rs/crates/instant-server/src/self_hosted_secrets.rs
[docker-entrypoint.sh]: ../server-rs/docker-entrypoint.sh
