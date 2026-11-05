# Contributing to BackendRelay

Thanks for helping. BackendRelay is a fork of InstantDB with a Rust backend. It is not affiliated with InstantDB, so please report BackendRelay problems here, not to the upstream project.

## Proposing changes

- **Bugs:** open an issue with the version or commit, what you did, what you expected, and what happened. Include a minimal reproduction if you can.
- **Features and larger changes:** open an issue first so the approach can be agreed before you write a lot of code.
- **Pull requests:** fork the repository, branch from the default branch, and open a PR describing what changed and why. Link the related issue.

## Making a good pull request

- Keep each PR to one logical change. Do not combine refactors, formatting churn, or dependency bumps with unrelated fixes.
- Add or update tests that cover the change. Behavior that involves PostgreSQL needs a database-backed test. Those tests skip silently unless `TEST_DATABASE_URL` is set, so run them through `server-rs/run-postgres-acceptance.sh`.
- Do not edit migrations that are already committed. Add a new one instead (see [Database migrations](docs/development.md#database-migrations)).
- Do not commit secrets, credentials, `.env` files, or hostnames from your own deployments.

## Checks to run before opening a PR

Run the checks that match what you changed. [docs/development.md](docs/development.md) explains each one.

| You changed | Run |
| --- | --- |
| Anything | `make verify-quick` (repository root) |
| Rust (`server-rs/`) | `cargo fmt --all` and the relevant `cargo test --release --locked -p <crate>` (from `server-rs/`); `bash server-rs/run-postgres-acceptance.sh …` for database behavior |
| Client (`client/`) | `pnpm install --frozen-lockfile && pnpm build-packages && pnpm test:local` and `pnpm check-format` (from `client/`) |
| Dashboard or site (`client/www/`) | `pnpm --filter backendrelay-www check` and `pnpm --filter backendrelay-www test` (from `client/`) |
| Broad or cross-cutting | `make verify-local` (repository root), after you have finished editing |

In the PR description, list the checks you ran.

## Coding conventions

- Match the style, naming, and structure of the surrounding code.
- Format Rust with `rustfmt` using the pinned toolchain (`cargo fmt --all` inside `server-rs/`).
- Format TypeScript, JavaScript, JSON, and Markdown under `client/` with Prettier (`pnpm format` inside `client/`).
- Do not add `unsafe` Rust (the workspace forbids it), and do not add new Clippy warnings.
- Do not hand-edit generated files such as `client/www/src/routeTree.gen.ts`.

## Security issues

Do not report vulnerabilities in public issues or pull requests. Follow the private reporting process in [SECURITY.md](SECURITY.md).

## License

BackendRelay is licensed under the Apache License 2.0 (see [LICENSE.md](LICENSE.md)). By submitting a contribution, you agree that it is licensed under the same terms.
