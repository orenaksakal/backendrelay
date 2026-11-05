#!/usr/bin/env bash
# Register this repository's containerized self-hosted runner and start it.
# Run from the host as the Docker user; see README.md.
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

fail() {
    echo "ci-runner: $*" >&2
    exit 1
}

for command in gh git docker; do
    command -v "$command" >/dev/null 2>&1 \
        || fail "required command is not installed: $command"
done

if [[ -z "${RUNNER_REPO:-}" ]]; then
    remote=$(git remote get-url origin) \
        || fail "cannot read the origin remote; set RUNNER_REPO=owner/repo"
    case "$remote" in
        git@github.com:*) RUNNER_REPO=${remote#git@github.com:} ;;
        ssh://git@github.com/*) RUNNER_REPO=${remote#ssh://git@github.com/} ;;
        https://github.com/*) RUNNER_REPO=${remote#https://github.com/} ;;
        *) fail "cannot derive owner/repo from origin ($remote); set RUNNER_REPO=owner/repo" ;;
    esac
    RUNNER_REPO=${RUNNER_REPO%.git}
fi

[[ "$RUNNER_REPO" =~ ^[^/]+/[^/]+$ ]] \
    || fail "RUNNER_REPO must look like owner/repo"

RUNNER_WORK_DIR="${RUNNER_WORK_DIR:-$HOME/gh-runner-work}"
[[ "$RUNNER_WORK_DIR" = /* ]] \
    || fail "RUNNER_WORK_DIR must be an absolute path (it is also the path inside the container)"
[[ "$RUNNER_WORK_DIR" != "$HOME" ]] \
    || fail "RUNNER_WORK_DIR must be a dedicated directory, not HOME itself"
repo_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
if [[ -n "$repo_root" ]]; then
    case "$RUNNER_WORK_DIR" in
        "$repo_root"|"$repo_root"/*)
            fail "RUNNER_WORK_DIR must be outside the repository checkout"
            ;;
    esac
fi
mkdir -p "$RUNNER_WORK_DIR"
# The container runs as the image's uid 1001. The owner cannot chown a host
# directory to that uid without root, so grant write access on this dedicated
# CI-only directory. Do not point RUNNER_WORK_DIR at a checkout or at $HOME.
chmod 0777 "$RUNNER_WORK_DIR"

if [[ -z "${DOCKER_GID:-}" && -S /var/run/docker.sock ]]; then
    DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
fi

export RUNNER_REPO_URL="https://github.com/${RUNNER_REPO}"
export RUNNER_WORK_DIR
export RUNNER_NAME="${RUNNER_NAME:-framework-desktop}"
export DOCKER_GID="${DOCKER_GID:-}"

RUNNER_TOKEN=$(gh api -X POST \
    "repos/${RUNNER_REPO}/actions/runners/registration-token" --jq .token)
[[ -n "$RUNNER_TOKEN" ]] || fail "gh returned an empty registration token"
# Pass the token through the environment only: compose.yml forwards it to the
# container, and entrypoint.sh consumes it on the first start.
export RUNNER_TOKEN

docker compose -f "$script_dir/compose.yml" up -d --build

echo "runner ${RUNNER_NAME} registered for ${RUNNER_REPO}"
echo "follow the log with: docker compose -f ${script_dir}/compose.yml logs -f runner"
