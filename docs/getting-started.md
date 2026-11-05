# Getting started

This guide runs BackendRelay on your machine and connects an app to it. BackendRelay is a fork of [InstantDB](https://github.com/instantdb/instant) whose backend is reimplemented in Rust (`server-rs/`). It is not affiliated with InstantDB. Apps use the standard `@instantdb/*` client SDKs, pointed at your server with the `apiURI` and `websocketURI` options.

For production deployments, see [self-hosting.md](self-hosting.md).

## Choose a path

| Path | Cloud credentials | Use it for |
| --- | --- | --- |
| [Docker test stack](#run-the-docker-test-stack) (`self-hosting/docker-compose.test.yml`) | None. RustFS (an S3-compatible emulator) provides object storage. | **Recommended first run.** It builds the server and dashboard from source and runs them with PostgreSQL and RustFS. |
| [Server from source](#alternative-run-the-server-from-source) (`cargo`) | None | Working on the Rust server. |
| `self-hosting/docker-compose.local.yml`, `docker-compose.yml` | Cloudflare R2 required | Production-like setups. See [self-hosting.md](self-hosting.md). |
| `self-hosting/scale-local/stack.sh` | None | Linux load testing only (pins CPUs, needs `mise` and `taskset`). It is not a development setup. |

## Run the Docker test stack

### Prerequisites

- Docker Engine with the Compose v2 plugin (`docker compose`).
- `curl`.
- Free ports on `127.0.0.1`: 8888 (API), 3000 (dashboard), 8890 (PostgreSQL), 9000 (RustFS). You can change them with `INSTANT_SERVER_PORT`, `INSTANT_DASHBOARD_PORT`, `INSTANT_POSTGRES_PORT` and `INSTANT_S3_TEST_PORT`. If you change the server or dashboard port, also set `INSTANT_BACKEND_URL` or `INSTANT_DASHBOARD_URL` to match.
- Enough memory for Docker to build both images. The first build compiles the Rust server in release mode and builds the dashboard with a 4 GB Node heap limit, so it takes a while.
- Node.js, for the app you connect.

The stack uses well-known default credentials (`instant`/`pass` for PostgreSQL, `testaccesskey`/`testsecretkey` for RustFS) and binds only to `127.0.0.1`. Use it for local work only.

> [!IMPORTANT]
> Don't keep a `self-hosting/.env` copied from `.env.example` while you use this stack. Compose loads that file automatically. Its `INSTANT_EMAIL_PROVIDER=cloudflare` makes the server refuse to start, because the test stack doesn't pass the Cloudflare email settings to the server.

### 1. Build, migrate, and start

```sh
cd self-hosting

# Required. The compose file passes an empty value when this is unset, and the
# server rejects an empty URL at startup. Browsers also use it for file URLs.
export S3_PUBLIC_ENDPOINT=http://localhost:9000

docker compose -f docker-compose.test.yml build server www

# Starts PostgreSQL and RustFS, creates the buckets, then applies migrations.
docker compose -f docker-compose.test.yml run --rm server migrate up

docker compose -f docker-compose.test.yml up -d server www
```

Run the `build` step before `up`. The image names default to `backendrelay/server:local` and `backendrelay/dashboard:local`. If those images aren't present locally, Compose may try to pull them instead of building.

The server checks migrations when it starts but never applies them. That's why `migrate up` is a separate step. The first `migrate` run also generates the server's encryption and signing keys into the `server_secrets` volume. Later `serve` runs reuse them.

Check that the server is healthy:

```sh
curl -s http://localhost:8888/health/system
# {"wal":"ok"}
```

| Service | URL |
| --- | --- |
| API and WebSocket | `http://localhost:8888`, `ws://localhost:8888/runtime/session` |
| Dashboard | `http://localhost:3000/dash` |
| S3 (RustFS) | `http://localhost:9000` |
| PostgreSQL | `postgresql://instant:pass@127.0.0.1:8890/instant` |

### 2. Create an app

**In the dashboard:**

1. Open `http://localhost:3000/dash`, enter any email address, and select **Send Code**.
2. This stack has no email provider, so the server logs the code instead of sending it:

   ```sh
   docker compose -f docker-compose.test.yml logs server | grep verification_code
   ```

3. Enter the code and select **Verify Code**.
4. Enter an **Application name** and select **Create application**.

The dashboard header shows the **Public App ID**. The app's **Admin** tab shows its **Admin token**. Only the app's Owner and Admin members (or a platform token with the `apps-write` scope) see the token; collaborators see the rest of the app but never the admin token. Use the admin token only on servers, never in browser code.

**From the command line:** the API can also create an ephemeral app without signing in. Ephemeral apps are deleted after 14 days unless someone claims them.

```sh
curl -s -X POST http://localhost:8888/dash/apps/ephemeral \
  -H 'content-type: application/json' \
  -d '{"title":"my-first-app"}'
```

The response contains `app.id` and `app["admin-token"]`.

### 3. Connect an app

Install the SDK for your framework, for example `npm install @instantdb/react`. Then set `apiURI` and `websocketURI` to your server:

```tsx
import { init, id } from '@instantdb/react';

const db = init({
  appId: '<your-app-id>',
  apiURI: 'http://localhost:8888',
  websocketURI: 'ws://localhost:8888/runtime/session',
});

export function Todos() {
  const { isLoading, error, data } = db.useQuery({ todos: {} });
  if (isLoading) return null;
  if (error) return <p>{error.message}</p>;
  return (
    <>
      <button
        onClick={() =>
          db.transact(db.tx.todos[id()].update({ text: 'Hello', createdAt: Date.now() }))
        }
      >
        Add
      </button>
      <ul>
        {data.todos.map((todo) => (
          <li key={todo.id}>{todo.text}</li>
        ))}
      </ul>
    </>
  );
}
```

`@instantdb/core` takes the same options: `init({ appId, apiURI, websocketURI })`. On a server, the Admin SDK needs the admin token and `apiURI`:

```ts
import { init } from '@instantdb/admin';

const adminDb = init({
  appId: '<your-app-id>',
  adminToken: process.env.INSTANT_APP_ADMIN_TOKEN,
  apiURI: 'http://localhost:8888',
});
```

The server accepts cross-origin requests on its runtime routes, so an app on another local port, such as a Vite dev server, can connect directly. In production, use `https://` and `wss://` URLs. The repository's acceptance suites run against the SDK sources in `client/packages`, which track SDK version `v1.1.0` (`client/packages/version/src/version.ts`).

### Stop or reset

```sh
docker compose -f docker-compose.test.yml down            # keep data
docker compose -f docker-compose.test.yml down --volumes  # delete database, files, and keys
```

To run the dashboard live journey against a disposable copy of this stack, run `make verify-dashboard-browser` from the repository root. It requires `node`, `python3`, and `pnpm` (or `mise`), and it installs Playwright Chromium.

## Alternative: run the server from source

### Prerequisites

- `rustup`. The toolchain is pinned to Rust 1.98.1 in `server-rs/rust-toolchain.toml`. Run Cargo from inside `server-rs/` so rustup selects it. `--manifest-path` alone doesn't.
- A C toolchain, `cmake`, and `perl`. The server `Dockerfile` installs these for the build.
- PostgreSQL 17 with the `wal2json` output plugin and `wal_level=logical`.
- An S3-compatible object store. The server requires storage settings at startup.

The easiest way to get PostgreSQL and storage is to start only those services from the test stack:

```sh
cd self-hosting
docker compose -f docker-compose.test.yml up -d postgres s3 createbuckets
```

To use your own PostgreSQL 17 instead, install `wal2json`, either from the PGDG `postgresql-17-wal2json` package or from source. `server-rs/run-postgres-acceptance.sh` shows a source build of wal2json 2.6 against a `mise`-installed PostgreSQL 17.11. Set at least `wal_level = logical`, `max_replication_slots = 20`, `max_wal_senders = 20`, and a bounded `max_slot_wal_keep_size` such as `4GB`. [self-hosting.md](self-hosting.md#postgresql-requirements) lists the full requirements.

### Configure, migrate, and serve

```sh
cd server-rs

export DATABASE_URL='postgresql://instant:pass@127.0.0.1:8890/instant?sslmode=disable'
export WAL_HISTORY_STORAGE=pg
export INSTANT_BACKEND_URL=http://localhost:8888
export INSTANT_DASHBOARD_URL=http://localhost:3000
export S3_ENDPOINT=http://127.0.0.1:9000
export S3_PUBLIC_ENDPOINT=http://localhost:9000
export INSTANT_S3_REGION=us-east-1
export INSTANT_S3_ACCESS_KEY_ID=testaccesskey
export INSTANT_S3_SECRET_ACCESS_KEY=testsecretkey
export S3_BUCKET=instant-bucket
export S3_APP_BACKUPS_BUCKET=instant-backups

cargo build --locked --release -p instant-server

# Generate the keys once. Keep the file outside the repository.
mkdir -p ~/.config/backendrelay
(umask 077 && ./target/release/instant-server generate-secrets > ~/.config/backendrelay/dev-secrets.env)
set -a; . ~/.config/backendrelay/dev-secrets.env; set +a

./target/release/instant-server migrate up   # or: cargo run --locked -p instant-server -- migrate up
./target/release/instant-server serve        # listens on 0.0.0.0:${PORT:-8888}
```

Set `S3_APP_BACKUPS_BUCKET` for app backups. WAL history stays in PostgreSQL unless you set `WAL_HISTORY_BUCKET_NAME`; there are no hosted-service bucket defaults. `serve` also requires the three keys printed by `generate-secrets`: `INSTANT_AEAD_KEYSET_JSON`, `INSTANT_WEBHOOK_KEYSET_JSON`, and `INSTANT_RATE_LIMIT_HMAC_KEY`. Keep using the same keys, because data encrypted with one keyset can't be read with another.

Then create an app with the ephemeral-app `curl` command above and connect your app as shown in [step 3](#3-connect-an-app).

## Next steps

- [../examples/tanstack-start](../examples/tanstack-start) and [../examples/expo](../examples/expo): runnable apps already pointed at a self-hosted server.
- [self-hosting.md](self-hosting.md): production deployment, PostgreSQL, storage, email, TLS, backups, and monitoring.
- [configuration.md](configuration.md): all environment variables.
- [managed-postgres.md](managed-postgres.md): managed PostgreSQL providers.
- [performance.md](performance.md): measured capacity.
