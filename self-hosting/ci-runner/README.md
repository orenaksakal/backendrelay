# Containerized self-hosted CI runner

CI and nightly workflows target `runs-on: [self-hosted, linux, x64,
backendrelay]`. The desktop has Docker but no passwordless sudo, so the runner
runs in the container defined here instead of as a dedicated `gh-runner` Unix
user. The container is Ubuntu 24.04 and is isolated from the owner's home
directory, SSH agent, and 1Password/Bitwarden sessions.

Files:

| File | Purpose |
| --- | --- |
| `Dockerfile` | Ubuntu 24.04 runner image with the job toolchains and Playwright system libraries. |
| `compose.yml` | One `runner` service with only the Docker socket, the work directory and three named volumes. |
| `entrypoint.sh` | Configures the runner on first start, then `exec`s `run.sh`. |
| `register.sh` | Mints a registration token with `gh` and starts the container. |
| `unregister.sh` | Stops the container and deletes the runner from GitHub. |

`.github/RUNNER.md` holds the repository-level setup and hardening rules.

## How it works

- The image starts `entrypoint.sh` as the image's `runner` user (uid 1001). On
  the first start it runs `config.sh` with a short-lived registration token
  supplied through `RUNNER_TOKEN`; afterwards the `.runner` and `.credentials*`
  files live in the `backendrelay-ci-runner_runner-state` volume, so restarts
  do not need a token. The token is never written to the image or the
  repository.
- The only mounts are `/var/run/docker.sock`, `${RUNNER_WORK_DIR}` at the same
  absolute path inside the container, and the `runner-state`, `runner-mise-data`
  and `runner-home-cache` volumes. `${HOME}` itself is never mounted.
- The work directory must have the same absolute path on the host and in the
  container: jobs start sibling Compose projects that bind-mount repository
  paths resolved by the host Docker daemon.
- Jobs use the host Docker daemon through the socket, so the container gets the
  host's `docker` group gid (`DOCKER_GID`, default `973`) with `group_add`.

## Prerequisites

On the desktop:

- Docker Engine with Compose v2, and the owner in the `docker` group (or
  otherwise able to use the socket).
- `gh`, authenticated with repository admin (minting a runner registration
  token requires admin).
- `git` and `curl`.

No other host tooling is needed. The image includes the apt packages the jobs
use (`build-essential`, `pkg-config`, `python3`, `python3-yaml`, `jq`, `age`,
the PostgreSQL build headers), Docker CLI with the Compose v2 and buildx
plugins, mise with Node 24 and pnpm 10.34.9, `cargo-deny` 0.20.2, `cosign` for
release signing, and Playwright's Chromium/Firefox/WebKit system dependencies.
Rust is not installed in the image; the workflows install the project
toolchain themselves.

## Register

```sh
bash self-hosting/ci-runner/register.sh
```

The script derives `owner/repo` from `origin`. Override any of:

| Variable | Default | Meaning |
| --- | --- | --- |
| `RUNNER_REPO` | from `origin` | `owner/repo`. |
| `RUNNER_WORK_DIR` | `$HOME/gh-runner-work` | Absolute host and container path for checkouts. Keep it out of the repository and out of `$HOME` itself. |
| `RUNNER_NAME` | `framework-desktop` | Runner name in GitHub; `--replace` takes over an existing name. |
| `DOCKER_GID` | socket group, else `973` | Host `docker` group gid. |

`register.sh` creates the work directory, grants write access for the
container's uid (`chmod 0777`: the owner cannot `chown` to uid 1001 without
root), mints the token, and runs `docker compose up -d --build` with the token
in the environment only.

Verify:

```sh
docker compose -f self-hosting/ci-runner/compose.yml ps
docker compose -f self-hosting/ci-runner/compose.yml logs -f runner
gh api repos/<owner>/<repo>/actions/runners \
  --jq '.runners[] | {name, status, busy}'
```

The runner registers with the `backendrelay` label. The mise toolchains and the
`/home/runner/.cache` volume persist, so repeated jobs are faster.

## Update

- Change `MISE_VERSION`, `CARGO_DENY_VERSION`, `COSIGN_VERSION`, the Node/pnpm
  pins or the apt package list in `Dockerfile`, then rerun `register.sh` (it
  rebuilds). The mise and cache volumes survive. Set `COSIGN_SHA256` to the
  sha256 of the pinned cosign-linux-amd64 download; the kit test fails while
  the `REPLACE_WITH_SHA256` placeholder is still present.
- When changing the runner release, update the `FROM` tag and digest in
  `Dockerfile`, the image tag in `compose.yml`, and `.github/RUNNER.md`. The
  runner config volume contains the old runner install: run `unregister.sh`,
  then register again with the new image.
- `PLAYWRIGHT_HOST_PLATFORM_OVERRIDE: ubuntu24.04-x64` in the workflows is
  harmless (the container is Ubuntu 24.04) and is kept so an older host still
  works.

## Release images

`.github/workflows/release-images.yml` runs
`bash self-hosting/release-images.sh` for `v*` tags and manual dispatches. It
needs three one-time owner settings:

- `COSIGN_PRIVATE_KEY` and `COSIGN_PASSWORD` repository secrets. Generate the
  key pair outside the repository with `cosign generate-key-pair`, commit only
  the public key as `self-hosting/cosign.pub`, and keep the private key in the
  password manager.
- arm64 emulation on the Docker host for the target-platform runtime stages:
  `docker run --privileged --rm tonistiigi/binfmt --install arm64`.
- GHCR packages named `backendrelay-server` and `backendrelay-dashboard` that
  stay private.

The workflow builds linux/amd64 and linux/arm64 indexes, pushes `<tag>` and
`sha-<short>`, signs each index digest with the key pair and no transparency
log, then verifies both platforms, the signature, the SBOM and the provenance
before recording the digests in the `release-digests.json` artifact. Run the
same script locally without `--push` to build both platforms and load the host
architecture.

## Unregister

```sh
bash self-hosting/ci-runner/unregister.sh
```

Pass the same `RUNNER_REPO` and `RUNNER_NAME` used to register. It runs
`docker compose down`, deletes the runner by name through the API, and removes
only the runner-state volume. The mise and build-cache volumes stay.
Re-registering after an API-side removal without this script leaves a stale
`.runner`; `register.sh` will not replace it, so remove the config volume in
that case.

## Hardening

- **Docker socket is root-equivalent.** Any job that can reach
  `/var/run/docker.sock` can control the host's Docker daemon. This is
  acceptable only because the repository is private and only the owner's
  workflows run here.
- Keep the repository private and never add `pull_request_target` or run jobs
  in a fork's context. The `if:` guard on every workflow job enforces that.
- Give the runner no production credentials: no SSH keys, no 1Password or
  Bitwarden session, no R2 keys, and no repository secrets beyond what the
  workflows need (they need none). Nothing from the owner's home is mounted.
- Keep `security_opt: [no-new-privileges:true]` and do not add mounts. In
  particular, do not mount a home directory, the SSH agent socket, or the
  password-manager sockets.
- Jobs check out with `clean: true` and `persist-credentials: false`, and Docker
  lanes finish with `docker system prune -f --filter until=24h`.
- WebKit's system libraries are in the image, so the old host step
  `sudo npx playwright install-deps webkit` is gone.
- If the repository becomes public, move pull-request jobs to GitHub-hosted
  runners; keep this runner for the owner's push builds only.

## Check the kit without Docker

```sh
bash -n self-hosting/ci-runner/*.sh
bash self-hosting/test-ci-runner-kit.sh
```

The test is a static contract: it checks the pinned runner digest, the exact
Compose mount set, the token handling in the entrypoint and register script,
and the shell syntax.
