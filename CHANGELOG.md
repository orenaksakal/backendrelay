# Changelog

All notable changes to BackendRelay are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Releases use date-based tags, for example `v2026.11.1`.

## [Unreleased]

### Added

- `GET /health/version` reports the build's `version`, `git_sha` and
  `migration_version`, and `instant-server --version` prints the same version and
  revision. Startup logging includes both.
- Key rotation: `generate-secrets --rotate` and `reencrypt` re-encrypt stored
  secrets after a key change.
- A `flags` CLI and a seeded, locked config app for deployment-wide settings.
- `audit` for read-only app-rules hygiene findings.
- Black-box cross-app isolation acceptance coverage and scale-local failure
  drills.
- Continuous WAL archiving and point-in-time restore via pgBackRest, with an
  online backup workflow and restore drills.

### Changed

- The server keeps running through PostgreSQL restarts: replication streams
  reconnect, dead pooled connections are discarded, and invalidation health
  recovers without a process restart.
- A binary may serve a newer schema while the newer migrations are marked
  additive; otherwise it still refuses to start.
- Database waits are bounded and one large app can no longer starve the others.
- Realtime connections drain gracefully on shutdown and clients reconnect with
  jittered backoff.
- Result-cache memory is capped across apps, with memory alerts and a bounded
  table-growth budget.
- Storage uploads enforce a size cap, safe content headers, quota and URL
  lifetime.
- Background worker failures are visible and stuck jobs are reclaimed.
- The server runs as a non-superuser database role in a hardened container, and
  only the proxy container is trusted for client IPs.
- Only two upstream client examples are kept, alongside a cleaned-up tree that
  no longer ships the AWS-hosted feature, upstream billing, website content or
  one-off diagnostics.
- Rust toolchain moved to 1.98.1.

### Fixed

- Untrusted input can no longer panic the server.
- Driver, OAuth transport and internal error text no longer reaches API clients.
- Magic codes now use the full digit alphabet, allow one live code, enforce
  lockout and expire after 15 minutes.
- OAuth audiences are always enforced, with tighter nonce, origin and metadata
  checks.
- Permission rules compile after the request's database lease is released.
- Dashboard signup and ephemeral app creation are closed by default, and email
  settings are limited to dashboard users.

### Security

- Request bodies, request duration, sessions, subscriptions and query size are
  limited.
- Dashboard refresh tokens are hashed, and admin tokens are no longer shown to
  collaborators.
- Active-session stats require superadmin, and counter tokens are compared in
  constant time.
- Magic-code email is rate-limited.
- CI runs dependency audits and container hardening checks.

### Infrastructure

- CI runs on a containerized self-hosted GitHub Actions runner; push runs finish
  instead of cancelling, live tests fail instead of silently skipping when a
  database is required, and the dashboard has a blocking runtime smoke test.
- The test stack uses RustFS instead of MinIO and builds its own PostgreSQL
  image.
