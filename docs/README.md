# BackendRelay documentation

| Guide | What it covers |
|---|---|
| [Getting started](getting-started.md) | Run BackendRelay locally (Docker test stack or from source) and connect an app |
| [Self-hosting](self-hosting.md) | Production deployment with Docker Compose; PostgreSQL, storage, email, TLS, backups, monitoring |
| [Operations](operations.md) | Alerts and app controls, secrets and key rotation, drills, app onboarding, and the operating cadence |
| [Configuration](configuration.md) | Every environment variable and command-line option of the server |
| [Managed PostgreSQL](managed-postgres.md) | Database requirements and PlanetScale for Postgres setup |
| [Architecture](architecture.md) | How the Rust server executes queries, enforces permissions and delivers realtime updates |
| [Performance](performance.md) | Load-test methodology, measured capacity and sizing guidance |
| [Development](development.md) | Toolchains, building, tests and verification gates for contributors |
| [Client SDK](sdk.md) | Install, required `apiURI` configuration, release and rollout procedure, compatibility promise |
| [Publishing the SDK](sdk-publishing.md) | npm scope, versions, provenance, Verdaccio rehearsal and the CI tag flow |

Client SDK usage (queries, transactions, auth, permissions syntax) follows the
InstantDB SDKs; see [Client SDK](sdk.md) for installation and the configuration
every app must set, and point them at your server with the `apiURI` option as
shown in [Getting started](getting-started.md).

Also see [CONTRIBUTING.md](../CONTRIBUTING.md) and [SECURITY.md](../SECURITY.md).
