# Development

This guide is for people working on the BackendRelay source. BackendRelay is a fork of [InstantDB](https://github.com/instantdb/instant) with the backend re-implemented in Rust. It is not affiliated with InstantDB. Crates, packages, binaries and environment variables still use upstream names (`instant-*`, `@instantdb/*`, `INSTANT_*`); only the packages published to npm carry our own scope, see [Publishing the SDK](sdk-publishing.md).

[`AGENTS.md`](../AGENTS.md) at the repository root is a short, maintained list of toolchain facts and gotchas. If this guide and `AGENTS.md` disagree, trust `AGENTS.md` and open an issue.

## Repository layout

There is no root Cargo or pnpm workspace. Each top-level directory has its own toolchain.

| Path | Contents |
| --- | --- |
| [`server-rs/`](../server-rs) | Rust server: a Cargo workspace (`crates/`), the vendored `cel` crate (`vendor/cel`), the `Dockerfile`, and test harness scripts. |
| [`client/`](../client) | pnpm workspace containing the SDKs (`packages/`), a React Native development sandbox (`sandbox/react-native-expo`), and the website and dashboard (`www/`). |
| `client/packages/` | TypeScript SDKs and tools: `core`, `react`, `react-common`, `react-native`, `react-native-mmkv`, `expo-sqlite`, `admin`, `platform`, `cli`, `components`, `resumable-stream`, `webhooks`, `version`. |
| `client/www/` | Two apps in one package (`backendrelay-www`). `site/` is an Astro site and `src/routes/` is a TanStack Start dashboard. It is not Next.js. |
| [`examples/`](../examples) | Two standalone example apps (`tanstack-start`, `expo`). They install published `@instantdb/*` packages from npm, pin them to `1.0.66`, and are not part of the pnpm workspace. |
| [`self-hosting/`](../self-hosting) | Docker Compose files, operator tooling, verification gates (`verify-local.py`), acceptance harnesses, and local load tests (`scale-local/`). |
| [`docs/`](.) | Documentation. |

### Rust crates

The executable starts in `server-rs/crates/instant-server/src/main.rs`. `production_routes.rs` in the same crate wires PostgreSQL-backed services into the `instant-http` routers.

| Crate | Purpose |
| --- | --- |
| `instant-datalog` | Datalog query types, planning, and SQL rendering. Does not connect to a database. |
| `instant-instaql` | Validation and coercion of InstaQL queries, compilation to Datalog, system catalog, and invalidation predicates. |
| `instant-wal` | Logical-WAL primitives: PostgreSQL LSNs, wal2json v2 entries, transaction assembly, and invalidation topics. |
| `instant-transact` | Parsing, preparation, and execution of the transaction (write) protocol. |
| `instant-permissions` | Evaluation of CEL permission rules, plus rate limiting. |
| `instant-auth` | Runtime authentication: user and refresh tokens, magic codes, platform tokens, and OAuth. |
| `instant-pg` | PostgreSQL query execution, the logical replication stream and slots, statistics sketches, and Bring Your Own Postgres (BYOP) schema introspection. |
| `instant-realtime` | Realtime sessions, subscriptions, query cache, invalidation, rooms, and presence. |
| `instant-http` | HTTP and WebSocket protocol handling, middleware, and axum route assembly for the runtime, admin, dashboard, and platform APIs. |
| `instant-server` | The `instant-server` binary: configuration, migrations, PostgreSQL-backed service wiring, and background workers. |

## Toolchains

| Tool | Version and source | Notes |
| --- | --- | --- |
| Rust | 1.98.1, pinned in [`server-rs/rust-toolchain.toml`](../server-rs/rust-toolchain.toml) (with `clippy`, `rustfmt`, `llvm-tools-preview`) | Run Cargo from inside `server-rs/` so rustup reads the toolchain file. `--manifest-path` from another directory does not select it. A `RUSTUP_TOOLCHAIN` environment variable overrides the file; unset it if your shell sets one. |
| pnpm | 10.34.9, pinned by `packageManager` in `client/package.json` | Run pnpm inside `client/`. `corepack pnpm` also works. |
| Node.js | No repository-wide pin | `client/www` declares `"node": "22.x"`. The native SDK harness and the dashboard live harness use Node 24 (through mise). Use the version the entry point you are running asks for. |
| Python 3 | System | Needed by the verification gates and `self-hosting/` tooling. `make verify-quick` needs PyYAML: `pip install -r self-hosting/requirements-operator.txt`. |
| mise | Not pinned | No mise config file is checked in. Scripts call mise to install pinned tools: `postgres@17.11` (PostgreSQL acceptance, native SDK harness, `scale-local`) and `node@24` (native SDK harness, and the dashboard live harness when mise is present). |
| C toolchain, `make`, `git` | System | Used to build wal2json (and pg_hint_plan for `scale-local`) against the mise-installed PostgreSQL. |
| Docker with Compose | System | Only needed for `make verify-dashboard-browser` and the Compose stacks. |

## Building and running the server

Build from `server-rs/`:

```sh
cd server-rs
cargo build --locked --release -p instant-server
./target/release/instant-server --help
```

```text
usage: instant-server [serve | healthcheck | failover | generate-secrets | configure-byop app-id | flags list | get name | set name json | unset name | audit | migrate [up | down [steps] | force version]]
```

**Feature flags.** The server has no feature-gated cloud integrations and no AWS SDK dependencies. AWS-only settings such as `DATABASE_CLUSTER_ID` or `INSTANT_AEAD_KEYSET_KMS_KEY_URL` are startup errors. The container image ([`server-rs/Dockerfile`](../server-rs/Dockerfile)) builds with `--no-default-features`.

**Vendored `cel`.** `server-rs/Cargo.toml` patches `cel` to [`server-rs/vendor/cel`](../server-rs/vendor/cel). That directory holds cel 0.14.3 (MIT, from cel-rust) with local changes that keep permission-rule semantics compatible. It is excluded from the workspace. Change CEL behavior there, not by changing the registry version. Each patch, its motivation and its covering test are recorded in [`vendor/cel/PATCHES.md`](../server-rs/vendor/cel/PATCHES.md).

### Running locally

The server needs PostgreSQL 17 with `wal_level = logical` and the [wal2json](https://github.com/eulerto/wal2json) output plugin installed, because realtime invalidation reads a logical replication stream.

`serve` checks that migrations are current and refuses to start if they are not. It never applies them. Run `migrate up` explicitly first:

```sh
cd server-rs

# generate-secrets prints a shell-sourceable file with the OAuth AEAD keyset,
# webhook keyset, and rate-limit HMAC key. Keep it outside the checkout, readable
# only by you, and reuse it: data encrypted with one keyset cannot be read with another.
(umask 077 && ./target/release/instant-server generate-secrets > "$HOME/backendrelay-dev-secrets.env")
set -a; . "$HOME/backendrelay-dev-secrets.env"; set +a

export DATABASE_URL='postgresql://instant@127.0.0.1:5432/instant?sslmode=disable'
export WAL_HISTORY_STORAGE=pg   # keep WAL history in PostgreSQL, not object storage

./target/release/instant-server migrate up
./target/release/instant-server serve
```

Without other settings, the server runs in development mode on port 8888 (`PORT`). `INSTANT_BACKEND_URL` defaults to `http://localhost:<port>` and `INSTANT_DASHBOARD_URL` to `http://localhost:3000`. File storage and backups need S3-compatible settings (`S3_ENDPOINT`, `INSTANT_S3_REGION`, `S3_BUCKET`, `S3_APP_BACKUPS_BUCKET`, `INSTANT_S3_ACCESS_KEY_ID`, `INSTANT_S3_SECRET_ACCESS_KEY`).

[`self-hosting/scale-local/stack.sh`](../self-hosting/scale-local/stack.sh) is a complete, working example of this sequence. `stack.sh up` provisions a disposable PostgreSQL 17.11 with wal2json and pg_hint_plan, builds the release server, generates secrets in memory, migrates, and starts the server. It uses placeholder S3 values, which the server does not contact at startup. It is tuned for load tests (CPU pinning, large `shared_buffers`) and needs Linux tools such as `taskset` and `lscpu`. Stop it with `stack.sh down`.

### Website and dashboard

```sh
cd client
pnpm install --frozen-lockfile
pnpm build-packages
cd www
pnpm dev        # Astro site on :3000, TanStack Start dashboard on :3002
```

Running `make dev` from `client/` starts the site and dashboard together with the SDK packages in watch mode, through Turbo. Dashboard routes are registered in `client/www/virtual-routes.json`. Vite generates `src/routeTree.gen.ts`, so do not edit it by hand. Browser environment substitutions are allowlisted in `client/www/lib/public-env.mjs`. They keep their `NEXT_PUBLIC_*` names even though the app is not Next.js.

## Testing

### Rust tests

Run from `server-rs/`. The verification gates use `--release`, so using it locally reuses the same build artifacts.

```sh
cargo test --release --locked -p instant-server --lib
cargo test --release --locked -p instant-server --lib runtime_realtime::tests
cargo test --release --locked -p instant-permissions --lib
```

### Database-backed tests

Live PostgreSQL tests (the `live_*` test targets under `crates/*/tests/`, plus some unit-test modules) read `TEST_DATABASE_URL`. **If it is unset, they print `TEST_DATABASE_URL is not set; … skipped` to stderr and pass without touching PostgreSQL.** A green `cargo test` therefore says nothing about database behavior. The migration lifecycle test (`live_migrations`) uses `TEST_MIGRATION_DATABASE_URL` instead.

Use the acceptance harness from the repository root:

```sh
bash server-rs/run-postgres-acceptance.sh
```

It requires `mise`, `git`, `make`, `python3`, and `cargo`. It then:

1. Installs PostgreSQL 17.11 through mise and, if the plugin is missing, builds wal2json 2.6 into that installation.
2. Creates a disposable cluster with logical WAL, in a temporary directory, on a random localhost port.
3. Checks the replication-slot monitoring SQL, crash recovery of committed data and a logical slot, and a dump/restore round trip.
4. Runs the migration lifecycle (`live_migrations`).
5. Runs the whole workspace with `TEST_DATABASE_URL` set and `RUST_TEST_THREADS=1`. The run fails if any test reports that it skipped.

Extra arguments replace step 5 with a focused `cargo test`. Pass `--locked` yourself. The checks in steps 1–4 still run first.

```sh
bash server-rs/run-postgres-acceptance.sh --locked -p instant-server --test live_framework_root_frames
bash server-rs/run-postgres-acceptance.sh --locked -p instant-pg --test live_datalog
```

- `INSTANT_ACCEPTANCE_PROFILE=release` runs the Cargo tests with `--release` (the default is `dev`).
- `--storage-recovery-only` stops after the storage checks.
- Test output goes to `server-rs/target/postgres-acceptance.log`. On failure, the PostgreSQL log is copied to `server-rs/target/postgres-acceptance-postgres.log`.
- [`server-rs/run-rust-coverage.sh`](../server-rs/run-rust-coverage.sh) runs the same harness under `cargo-llvm-cov` 0.9.0 and enforces minimum coverage.

### Client tests

Run from `client/`:

```sh
pnpm install --frozen-lockfile
pnpm build-packages
pnpm test:local
```

- SDK package exports point to `dist/`, so run `pnpm build-packages` before anything that imports a workspace package by name. Turbo's `build` task builds dependencies first. Its `test:ci` task does not build anything.
- `pnpm test:local` runs every package's `test:ci` except Core's, then Core's Node unit tests (`test:unit`), Core's type contracts (`test:types`), and `scripts/mobile-dependency-contracts.test.cjs`. It needs no services.
- `pnpm test` also runs Core's Playwright `e2e` project. That needs browsers and a reachable backend, so it is not a service-free unit suite.

Focused checks:

```sh
pnpm --filter @instantdb/core exec vitest run --project node __tests__/src/datalog.test.ts
pnpm --filter @instantdb/core run test:types     # type contracts in packages/core/src/__types__/
pnpm --filter backendrelay-www check             # dashboard tsc
pnpm --filter backendrelay-www test              # dashboard script and vitest tests
```

There is no workspace-root `check` script.

### Repository verification gates

[`self-hosting/verify-local.py`](../self-hosting/verify-local.py) defines the gates. The root `Makefile` wraps it:

| Target | Gates |
| --- | --- |
| `make verify-quick` | Operator tooling unit tests (`self-hosting/test_local_*.py`), Node contract tests for recovery and release signatures, and `cargo fmt --all -- --check`. |
| `make verify-local` | Everything in `verify-quick`, plus release-mode Rust tests (`instant-permissions` and `instant-realtime` libs, selected `instant-server` lib tests, `instant-http` lib), dashboard `pnpm check` and `pnpm test`, Core Node tests and type contracts, and mobile dependency contracts. |

- Neither profile runs database-backed tests or browser acceptance. Run those separately (see above and below).
- Install and build the client (`pnpm install --frozen-lockfile && pnpm build-packages` in `client/`) before `make verify-local`.
- Gate output is discarded so that credentials are never captured. If a gate fails, rerun its command directly to see why.
- The report is written to `server-rs/target/verification/latest.json`, or to a new directory outside the checkout with `--evidence-dir`. `profileStatus` and `status` cover the selected profile.
- **Finish editing before you start a run.** The script hashes all tracked and untracked, non-ignored files before and after. Any change in between fails the `source-stability` gate.

### Browser and SDK acceptance

- **Dashboard live journey:** `make verify-dashboard-browser` runs [`self-hosting/run-dashboard-browser-acceptance.sh`](../self-hosting/run-dashboard-browser-acceptance.sh). It needs Docker, `curl`, Node, Python, and pnpm (or mise, which supplies Node 24 and pnpm 10.34.9). It builds the dashboard and starts a disposable Compose project from `self-hosting/docker-compose.test.yml` (PostgreSQL, RustFS and the Rust server), serves the dashboard with `vite preview`, and runs the `live` Playwright project against it. The journey signs in with a magic code read from the server log, then creates an app and exercises Explorer CRUD, schema add/delete/restore, rules and test-as, storage, webhooks, backup download, restore into a new app, token rotation and app deletion. The stack is removed afterward unless `INSTANT_DASHBOARD_ACCEPTANCE_KEEP_STACK=1` is set.
- **Dashboard runtime smoke test:** `bash self-hosting/run-dashboard-smoke.sh` is the cheap, blocking check that the built dashboard serves pages. It builds the server, dashboard, and PostgreSQL images, starts the same disposable Compose project, applies migrations, and then uses `curl` only: `GET /dash` must return HTTP 200 with the server-rendered dashboard document (never a Nitro SSR error payload such as `serverSsr.reserveStreamFastPath is not a function`), and `/` must answer (the self-hosted image redirects it to `/dash`). It catches broken dashboard builds that pass `tsc`, vitest, and `vite build`, needs Docker, `curl`, and Python 3, runs in CI in the `sdk-smoke` job, prints the container logs on failure, and removes the stack afterward unless `INSTANT_DASHBOARD_SMOKE_KEEP_STACK=1` is set.
- **Official SDK suites against a native server:** `bash self-hosting/run-official-sdk-e2e-native.sh --suite smoke|core|standard|full`. It provisions the server, the database, Node 24, and a Playwright Chromium, but needs a real Cloudflare R2 configuration (`R2_*` variables, or the git-ignored `self-hosting/.env`). Check prerequisites with `--preflight`. A bare Core `vitest run` is not a substitute for this setup.
- **Published SDK compatibility:** `bash self-hosting/run-official-sdk-e2e.sh --published-version <version> [--scope @backendrelay]` runs only the Core and Admin e2e suites against the published npm tarballs of that exact version. It uses the same disposable Compose stack but never builds `client/packages/*/dist`, so it proves the SDK that apps install. `--core-tarball`/`--admin-tarball` accept already-fetched tarballs for a local rehearsal. `node self-hosting/sdk-compat/resolve-pinned-sdk.mjs` prints the scope and version pinned by `examples/*`.
- **Cross-app isolation:** `bash self-hosting/run-cross-app-isolation-acceptance.sh` builds the server and PostgreSQL images, starts a disposable Compose project from `self-hosting/docker-compose.test.yml` (PostgreSQL and RustFS), and runs [`self-hosting/cross-app-isolation-acceptance.mjs`](../self-hosting/cross-app-isolation-acceptance.mjs). The probe creates two ephemeral apps and asserts that app A's refresh token, admin token, SSE session credentials, presigned storage URLs, room and topic traffic, stream subscriptions, and webhook configuration cannot reach app B, that the reserved deployment app id stays uncreatable, and that ephemeral app control is bound to its own admin token. It needs Docker, `curl`, Python 3, and Node 24 (or mise). The stack is removed afterward.

## Continuous integration

Workflows live in [`.github/workflows/`](../.github/workflows) and run on a GitHub Actions self-hosted runner on the owner's desktop, selected with the `backendrelay` label.

| Workflow | Jobs |
| --- | --- |
| [`ci.yml`](../.github/workflows/ci.yml) (push, pull request, manual) | `quick` (`make verify-quick`), `rust` (format check, `cargo test --release --locked --workspace`, the vendored `cel` tests, and the Clippy ratchet), `rust-pg` (`run-postgres-acceptance.sh` against disposable PostgreSQL), `client` (build, `pnpm test:local`, format check, dashboard type and unit tests), `sdk-smoke` (`run-official-sdk-e2e.sh --suite smoke`, the dashboard runtime smoke test, and the cross-app isolation acceptance). |
| [`nightly.yml`](../.github/workflows/nightly.yml) (02:17 UTC, manual) | `acceptance`, `sdk-e2e` (full official SDK suite), `sdk-compat` (Core and Admin e2e against the examples' pinned published tarballs and the oldest version reported in production), `dashboard-browser` (`make verify-dashboard-browser`), `restart-acceptance`, `upgrade-rollback` (previous image to candidate migration and rollback), `audit` (`make audit`) and `upstream-watch`, which records `npm view @instantdb/core dist-tags` in the job summary and never fails the run. |
| [`release-images.yml`](../.github/workflows/release-images.yml) (`v*` tags, manual) | `release` builds linux/amd64 and linux/arm64 server and dashboard images with SBOM and provenance, pushes them to GHCR, signs the manifest digests with the repository cosign key, verifies both platforms plus signature, SBOM and provenance, and uploads `release-digests.json`. |

The runner is a container on the owner's desktop, not a clean hosted VM: mise toolchains and build caches persist in named volumes, and the container reaches the host Docker daemon through `/var/run/docker.sock` (root-equivalent access), so it must carry no production credentials, Bitwarden or 1Password session, or SSH keys. It is isolated from the owner's home directory and registers with a label used only by this repository. The kit and registration helper live in [`self-hosting/ci-runner/`](../self-hosting/ci-runner/README.md); setup and the hardening rules (private repository, `push`/`workflow_dispatch` only, no `pull_request_target`, `clean: true` checkouts plus `docker system prune`, no fork PRs) are in [`.github/RUNNER.md`](../.github/RUNNER.md).

Clippy warnings are ratcheted rather than fixed yet: [`server-rs/clippy-baseline.txt`](../server-rs/clippy-baseline.txt) holds the current count from `cargo clippy --locked --workspace --all-targets --message-format=short`, and [`server-rs/clippy-ratchet.sh`](../server-rs/clippy-ratchet.sh) fails when the count grows. After removing warnings, lower the baseline with `bash server-rs/clippy-ratchet.sh --update`.

## Dependency audit

`make audit` fails the build on known vulnerabilities. It runs [`cargo deny`](https://embarkstudios.github.io/cargo-deny/) from `server-rs/` against [`deny.toml`](../server-rs/deny.toml) (advisories, licences, and sources) and then `pnpm audit --prod` from `client/`. Install the tools with `cargo install --locked cargo-deny` and `corepack enable pnpm`.

Anything suppressed below is listed with the reason it is safe today and the date by which it has to be re-checked; do not extend a date without re-reading the advisory. `pnpm audit` keeps the ignored advisories in its summary (`N ignored`), and `cargo deny -L info check advisories` prints the ignored Rust advisories with their reasons.

| Advisory | Package and path | Why it is ignored | Revisit by |
| --- | --- | --- | --- |
| GHSA-86w9-cpqp-85rv | `node-forge` 1.4.0 (`sandbox/react-native-expo` → `@expo/cli`) | No fixed release exists. Only the Expo CLI in the sandbox app reaches it; neither the server nor the dashboard ships it. | 2027-01-31 |
| GHSA-ch52-4w7c-c8xp | `http-cache-semantics` 4.2.0 (`www` → `astro`) | No fixed release exists; 4.2.0 is the newest. The cache layer only sees responses to our own origin, and `max-stale` is not configured. | 2027-01-31 |
| GHSA-gcqx-w48m-w2h4 | `deepmerge` 4.3.1 (`dashboard` → `codemirror-json-schema` → `json-schema-library`) | No fixed release exists. `json-schema-library` only deep-merges its own draft config defaults; the dashboard's schema text is parsed by `Draft04`/`Draft07` and never passed to `deepmerge`. Browser-side, low-integrity impact. | 2027-01-31 |
| GHSA-mrhx-c7xc-m3w5 | `zod` 3.25.76 (`sandbox/react-native-expo` → `@expo/cli`) | Fixed only in `zod` 4.6.6, and the Expo CLI is not compatible with that major. Build-time validation in the sandbox only. | 2027-01-31 |
| GHSA-rj75-hqrm-r3gf | `postcss-selector-parser` 6.1.3 (`sandbox/react-native-expo` → `nativewind` → Tailwind 3) | Fixed only in 7.1.6, and Tailwind 3 is built against the 6.x API. The CSS pipeline only runs when building the sandbox app. | 2027-01-31 |
| GHSA-wx4j-mvgx-mqwp | `satori` 0.12.1 (`www` → `@vercel/og`) | Fixed only in 0.33.5, a different API generation that the pinned `@vercel/og` 0.6.5 does not support. The only call site renders a hard-coded title with no untrusted SVG or CSS. | 2027-01-31 |

## Formatting and linting

- **Rust:** run `cargo fmt --all` from `server-rs/` with the pinned toolchain. There is no `rustfmt.toml`, so formatting follows the workspace edition (2024). The gate runs `cargo fmt --all -- --check`. Workspace lints forbid `unsafe_code` and enable Clippy `all` and `pedantic` at warning level. Clippy is not a verification gate, but do not add new warnings in code you touch.
- **TypeScript, JavaScript, JSON, and Markdown under `client/`:** Prettier 3.6.2 (`client/prettier.config.js`). Run `pnpm format` to write and `pnpm check-format` to check, from `client/`. Running `make lint` in `client/` is the same as `pnpm check-format`. It checks formatting only, not ESLint or TypeScript, so run the type checks above separately.

## Database migrations

Application SQL lives in [`server-rs/crates/instant-server/migrations/`](../server-rs/crates/instant-server/migrations). `build.rs` embeds every migration into the binary at compile time and fails the build unless:

- files are named `<version>_<name>.up.sql` and `<version>_<name>.down.sql` (for example `144_bounded_mcv_statistics.up.sql`);
- every version has both an up file and a down file;
- versions are contiguous from 1.

Migrations are append-only. The server records a SHA-256 checksum of each applied up script, and `serve` and `migrate` refuse to run if a recorded migration's SQL has changed. To change the schema, add the next version rather than editing an existing file. When you add one, update the count and latest-version assertions in the `embedded_migrations_are_complete_contiguous_and_paired` test in `crates/instant-server/src/migrations.rs`.

Apply and roll back with the binary (from `server-rs/`, with `DATABASE_URL` set):

```sh
cargo run --locked -p instant-server -- migrate up
cargo run --locked -p instant-server -- migrate down        # one step; `migrate down N` for N steps
```

`migrate force <version>` rewrites the recorded version without running SQL. It is for recovery only. The PostgreSQL acceptance harness exercises the full up/down lifecycle (`live_migrations`).

### Schema changes and rollbacks

Use an expand → migrate → contract sequence so that an older binary can keep running while newer additive migrations are applied:

1. **Expand.** Add the new tables, columns, indexes, or system catalog attributes in a migration whose `.up.sql` starts with the exact first line `-- compat: additive`. Expansion only adds schema, and nothing else in the migration may drop or rename objects, change column types, tighten constraints, or rewrite data in a way an older binary cannot tolerate.
2. **Migrate.** Deploy the new binary and move data and traffic to the new shape. During the overlap window the old binary may still be running: `serve` accepts a database schema newer than the binary only when every migration between the binary's latest version and the database version is recorded as additive.
3. **Contract.** Once the new binary is fully deployed and the old one can no longer be rolled back, remove the old objects in a later migration without the marker.

The additive flag is recorded per migration in `instant_migration_checksums.additive` when the migration is applied, so a binary that does not contain the newer SQL can still read it. Rows for migrations applied before this bookkeeping existed default to false, and existing migrations are not retroactively marked; a migration without the marker is treated as non-additive. Mark a migration as additive only when rolling the previous binary back onto the resulting schema is genuinely safe.

Rolling back the binary is safe only while that condition holds. `migrate down` and `migrate force` still refuse a schema newer than the binary, because rewriting a newer migration's recorded state with older SQL is unsafe. If `serve` reports `NewerDatabase`, either deploy the binary that produced the schema or restore a schema at or below this binary's latest migration.

System catalog attributes follow the same rule. `verify_system_catalog` tolerates attributes and identities it does not know, so an older binary can serve a catalog extended by a newer binary, but it still fails when an expected attribute or identity is missing or differs.

## Local load testing

[`self-hosting/scale-local/`](../self-hosting/scale-local) runs a multi-app load test on one Linux machine:

```sh
bash self-hosting/scale-local/stack.sh up        # disposable PostgreSQL + release server, CPU-pinned
bash self-hosting/scale-local/run.sh --apps 10 --users-per-app 1000
bash self-hosting/scale-local/stack.sh down
```

`run.sh` starts the stack if it is not running (`--fresh` forces a clean restart) and leaves it running afterward. It accepts `--apps`, `--users-per-app`, `--steady`, `--burst`, `--recovery`, `--ramp-per-second`, `--out`, and `--stack`. State and logs go to `$XDG_STATE_HOME/instant-scale-local/current` (override with `SCALE_STATE_DIR`). See `stack.sh` for the CPU-split and tuning variables, and [performance.md](performance.md) for methodology and results.
