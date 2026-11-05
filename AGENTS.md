# Repository guidance

## Boundaries and toolchains

- There is no root Cargo or pnpm workspace. Run Cargo inside `server-rs/` so rustup selects `rust-toolchain.toml` (Rust 1.98.1); `--manifest-path` alone does not select that toolchain. Run pnpm inside `client/` (pinned to 10.34.9).
- `client/www` declares Node 22.x, while the native SDK acceptance harness provisions Node 24. Follow the relevant entrypoint rather than assuming one repository-wide Node version.
- The Rust executable starts in `server-rs/crates/instant-server/src/main.rs`; `production_routes.rs` wires PostgreSQL-backed services into `instant-http` routers. The server is self-hosted and cloud-neutral: it has no AWS SDK dependencies. `cel` is patched to `server-rs/vendor/cel`, not the registry implementation.
- `client/www` is **not Next.js**: `site/` is Astro, while `src/routes/` is TanStack Start. `pnpm dev` there launches both (site :3000, dashboard :3002). Dashboard route registration lives in `virtual-routes.json`; Vite generates `src/routeTree.gen.ts`—do not hand-edit it. Browser env substitutions are explicitly allowlisted in `lib/public-env.mjs` despite retaining `NEXT_PUBLIC_*` names.

## Client checks

Run these from `client/`:

```sh
pnpm install --frozen-lockfile
pnpm build-packages
pnpm test:local
```

- SDK package exports point to `dist/`; build packages before consumers/tests that import workspace package names. Turbo's build task orders dependency builds; its test task does not depend on builds.
- Prefer `test:local` for non-browser checks. Plain `pnpm test` includes Core's Playwright e2e project and is not a service-free unit suite.
- Focused checks (also from `client/`):

```sh
pnpm --filter @instantdb/core exec vitest run --project node __tests__/src/datalog.test.ts
pnpm --filter @instantdb/core run test:types
pnpm --filter backendrelay-www check
pnpm --filter backendrelay-www test
pnpm check-format
```

- Core's runtime tests and type contracts are separate; type contracts live in `packages/core/src/__types__/`. There is no workspace-root `check` script. `make lint` here checks Prettier formatting, not ESLint or TypeScript.

## Repository verification

- From the repository root, `make verify-quick` runs operator/tooling contracts and Rust formatting; its Python tests need `self-hosting/requirements-operator.txt` (PyYAML).
- `make verify-local` adds selected release-mode Rust tests, dashboard type/tests, Core Node/type tests, and mobile dependency contracts. It does **not** run database-backed or browser acceptance.
- `self-hosting/verify-local.py` defines the exact gates. It suppresses subprocess output; rerun a failing gate's command directly for diagnostics. Reports go to `server-rs/target/verification/latest.json`. Finish edits before running it: source changes during verification fail the stability gate.
- For a focused Rust unit check, run `cargo test --release --locked -p instant-server --lib runtime_realtime::tests` from `server-rs/`.

## Database and acceptance gotchas

- Serving verifies migrations; it never applies them automatically. With the target environment configured, run `cargo run --locked -p instant-server -- migrate up` from `server-rs/` before serving.
- Application SQL lives in `server-rs/crates/instant-server/migrations/`. `build.rs` embeds contiguous numbered `<version>_<name>.up.sql` / `.down.sql` pairs. Append migrations rather than changing recorded SQL (checksums detect drift), and update the count/latest-version assertions in `src/migrations.rs` tests when adding one.
- Many Rust live tests return successfully without exercising PostgreSQL when `TEST_DATABASE_URL` is unset. From the root, use `bash server-rs/run-postgres-acceptance.sh`: it provisions disposable PostgreSQL 17.11 via mise, builds wal2json 2.6 if missing, runs migrations, and serializes live tests. Focus with e.g. `bash server-rs/run-postgres-acceptance.sh --locked -p instant-pg --test live_datalog`; the migration lifecycle still runs first.
- Native SDK/browser integration: `bash self-hosting/run-official-sdk-e2e-native.sh --suite smoke` provisions the server/database/browser but requires real R2 configuration; use `--preflight` to check prerequisites. Do not substitute a bare Core `vitest run` for this setup.
- `make verify-dashboard-browser` builds a disposable Docker Compose stack with an S3-compatible emulator (RustFS), serves the dashboard with `vite preview`, and runs the live Playwright journey (`client/dashboard/e2e/live`). Unlike this test stack, `self-hosting/docker-compose.local.yml` requires R2 credentials.
