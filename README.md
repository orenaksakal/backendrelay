# BackendRelay

<img src="client/www/public/relay.svg" alt="BackendRelay logo" width="64" height="64">

BackendRelay is a self-hosted realtime backend: a native Rust server on top of
PostgreSQL, a web dashboard, and compatibility with the
[InstantDB](https://github.com/instantdb/instant) client SDKs. Build apps with
relational queries, live subscriptions, optimistic updates, authentication,
permission rules, file storage and presence, and run the whole backend on your
own infrastructure.

## Features

- **Live queries:** InstaQL relational queries over a triple store in PostgreSQL;
  subscribed queries update in realtime when data changes.
- **Transactions** with optimistic updates in the client SDKs.
- **Permissions:** per-namespace rules written in CEL, enforced on every query,
  subscription and write.
- **Authentication:** magic codes by email, guest users, OAuth providers, and
  admin-issued refresh tokens.
- **File storage** on any S3-compatible service (S3, Cloudflare R2, and others).
- **Presence and rooms** for collaborative features.
- **Admin and platform HTTP APIs**, plus a web dashboard to manage apps, schemas,
  permissions and users.
- **Operations:** Prometheus metrics, health endpoints, migrations as an explicit
  step, and a load-testing harness.

## How it fits together

```
your app (@instantdb/react, /core, /admin, ...)
        │  HTTPS + WebSocket
        ▼
BackendRelay server (Rust, server-rs/) ──── S3-compatible storage
        │  SQL + logical replication (wal2json)
        ▼
PostgreSQL 17
```

Applications use the standard InstantDB SDKs and point them at your server:

```ts
import { init } from '@instantdb/react';

const db = init({
  appId: 'YOUR_APP_ID',
  apiURI: 'https://api.example.com',
  websocketURI: 'wss://api.example.com/runtime/session',
});
```

The server streams PostgreSQL changes through a logical replication slot, works
out which subscriptions each write affects, and re-runs only those queries,
sharing work between users who subscribe to the same data. See
[docs/architecture.md](docs/architecture.md).

## Getting started

- [Run it locally](docs/getting-started.md) and connect an app.
- [Self-host in production](docs/self-hosting.md) with Docker Compose.
- [Configuration reference](docs/configuration.md) for every environment variable.
- [Use a managed PostgreSQL service](docs/managed-postgres.md) such as PlanetScale.

## Performance

On a desktop-class 8-core machine (server and PostgreSQL sharing it), one
BackendRelay server handled **10,000 concurrent users across 10 apps**, including a
2× traffic burst, with every read, write and realtime update delivered in under
1.5 seconds. Methodology, results on other hardware and sizing guidance are in
[docs/performance.md](docs/performance.md).

## Repository layout

| Path | Contents |
|---|---|
| [`server-rs/`](server-rs/) | Rust server, query engine, permissions, realtime and tests |
| [`client/packages/`](client/packages/) | Client SDKs, admin SDK and CLI (InstantDB packages) |
| [`client/www/`](client/www/) | Website (Astro) and dashboard (TanStack Start) |
| [`self-hosting/`](self-hosting/) | Docker Compose files, operational tooling, load-test harness |
| [`examples/`](examples/) | Two runnable examples (TanStack Start and Expo) pointed at a self-hosted server |
| [`docs/`](docs/) | Documentation |

## Documentation

All guides are in [docs/](docs/README.md): getting started, self-hosting,
configuration, architecture, performance, managed PostgreSQL and development.

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/development.md](docs/development.md) for toolchains and tests. Report
security issues privately as described in [SECURITY.md](SECURITY.md).

## License and attribution

Apache License 2.0; see [LICENSE.md](LICENSE.md). BackendRelay is derived from
[InstantDB](https://github.com/instantdb/instant) (also Apache-2.0); upstream
authorship and history are available there. This repository is the public
snapshot of the Rust re-implementation. BackendRelay is not affiliated with or
endorsed by InstantDB. See [NOTICE.md](NOTICE.md).
