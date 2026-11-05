# Self-hosted CI runner

CI runs on a GitHub Actions self-hosted runner on the owner's desktop
(`framework-desktop`: Linux, on the tailnet). The workflows live in
[`workflows/ci.yml`](workflows/ci.yml) (push, pull request, manual),
[`workflows/nightly.yml`](workflows/nightly.yml) (nightly at 02:17 UTC plus
manual dispatch) and [`workflows/release-images.yml`](workflows/release-images.yml)
(`v*` tags and manual dispatch). Every job selects the runner with:

```yaml
runs-on: [self-hosted, linux, x64, backendrelay]
```

`backendrelay` is a label used only by this repository.

The runner runs in a container because the desktop has Docker but no
passwordless sudo for a dedicated `gh-runner` Unix user. The image, Compose
file, entrypoint and operator scripts live in
[`self-hosting/ci-runner/`](../self-hosting/ci-runner/README.md).

## Owner setup

1. Install Docker Engine with Compose v2 and make sure the owner can use the
   socket (`docker` group). Install `gh`, authenticate it with repository admin
   (minting a runner registration token requires admin), and keep `git` and
   `curl` available. No other host tooling is needed.
2. Register the runner with
   `bash self-hosting/ci-runner/register.sh`. It derives `owner/repo` from
   `origin` (override with `RUNNER_REPO=owner/repo`), mints a short-lived
   registration token, creates `${RUNNER_WORK_DIR:-$HOME/gh-runner-work}`, and
   starts the container with `docker compose up -d --build`. The token is
   passed only through the environment and is consumed on the first start; the
   persisted runner credentials live in a named volume.
3. Give the runner **no** production credentials: no Bitwarden or 1Password
   session, no SSH keys to the CAX41 host, no R2 keys. The only repository
   secrets are `COSIGN_PRIVATE_KEY` and `COSIGN_PASSWORD` for the release
   image workflow; nothing else needs them. The container has no access to the
   owner's home directory, SSH agent, or password-manager sockets.
4. Install no tools on the host beyond Docker, `gh`, `git`, and `curl`. The
   image carries the apt packages the jobs use, Docker CLI with the Compose v2
   and buildx plugins, mise with Node 24 and pnpm 10.34.9, `cargo-deny` 0.20.2,
   `cosign`, and Playwright's system libraries for Chromium, Firefox and
   WebKit. The workflows install Rust 1.98.0 themselves. Register arm64
   emulation once on the Docker host so the release workflow can run the
   target-platform runtime stages:
   `docker run --privileged --rm tonistiigi/binfmt --install arm64`.
5. Keep `RUNNER_WORK_DIR` as an absolute path that is **identical** on the host
   and in the container: jobs start sibling Compose projects whose bind mounts
   the host daemon resolves. Do not point it at the repository checkout, at
   `$HOME`, or at a directory holding credentials. `register.sh` grants the
   container's uid 1001 write access because the owner cannot `chown` to that
   uid without root.
6. Enable GitHub Actions for the repository and add branch protection on
   `main` that requires the `quick`, `rust`, `rust-pg`, `client`, and
   `sdk-smoke` checks.

To stop and remove the runner, run
`bash self-hosting/ci-runner/unregister.sh`. It also deletes the runner's
config volume so the next registration starts clean; the mise toolchains and
build caches are kept.

## Hardening

- The container mounts `/var/run/docker.sock`, so any job can control the
  host's Docker daemon. That is root-equivalent and is acceptable only because
  the repository is private and only the owner's workflows run here. Keep the
  container limited to the socket, the work directory, and the named runner
  volumes; do not add mounts.
- Keep the repository private. Workflows run on `push` to branches the owner
  controls, on `pull_request` from branches in this repository, and on
  `workflow_dispatch`. Never add `pull_request_target`, and never run jobs in a
  fork's context — the `if:` guard on every `ci.yml` job enforces that.
- Jobs check out with `clean: true` and `persist-credentials: false`; the Docker
  jobs in `ci.yml` and `nightly.yml` finish with
  `docker system prune -f --filter until=24h || true` (`if: always()`). The
  release job keeps the buildx cache between releases and logs out of GHCR
  instead.
- Do not put secrets in workflow files. The CI and nightly workflows need no
  token beyond the default `permissions: contents: read`; the release workflow
  reads `COSIGN_PRIVATE_KEY`/`COSIGN_PASSWORD` from repository secrets and
  gives them only to the signing step.
- The container is Ubuntu 24.04, so Playwright 1.58 supports it. The
  `PLAYWRIGHT_HOST_PLATFORM_OVERRIDE: ubuntu24.04-x64` env var stays harmless
  and is kept for an older host. WebKit's system libraries are in the image, so
  the old host `sudo npx playwright install-deps webkit` step is gone.
- Once the repository is public, move pull-request jobs to GitHub-hosted
  runners (free for public repositories) or keep this runner for push builds
  only.

## Scope

- `ci.yml`: `quick`, `rust`, `rust-pg`, `client`, `sdk-smoke`.
- `nightly.yml`: `acceptance`, `sdk-e2e`, `sdk-compat`,
  `dashboard-browser`, `restart-acceptance`, `upgrade-rollback`, `audit`,
  `upstream-watch`.
- `release-images.yml`: `release` (multi-arch build, push, sign and verify on
  `v*` tags or manual dispatch).
- Not wired yet: coverage-gated acceptance (P5.2) and `trivy image`. Add them
  once those steps exist.

The `sdk-compat` job runs the Core and Admin e2e suites against the published
tarballs the examples pin, then against the oldest version still reported in
production. The runner has no production credentials, so an operator reads the
oldest version from `backendrelay_sdk_sessions` with
`self-hosting/sdk-compat/oldest-sdk-version.mjs` and sets the repository
variable `SDK_COMPAT_OLDEST_VERSION` (and `SDK_COMPAT_OLDEST_SCOPE` when that
version came from upstream's `@instantdb` scope). A manual dispatch can pass
`oldest-version` instead.
