#!/usr/bin/env bash
# Stop the containerized runner and remove its registration from GitHub.
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

RUNNER_NAME="${RUNNER_NAME:-framework-desktop}"
# compose.yml interpolates these even for `down`; supply the same defaults so
# the stop does not print unset-variable warnings.
export RUNNER_REPO_URL="https://github.com/${RUNNER_REPO}"
export RUNNER_WORK_DIR="${RUNNER_WORK_DIR:-$HOME/gh-runner-work}"

docker compose -f "$script_dir/compose.yml" down

runner_ids=$(gh api --paginate "repos/${RUNNER_REPO}/actions/runners" \
    --jq ".runners[] | select(.name == \"${RUNNER_NAME}\") | .id")
if [[ -z "$runner_ids" ]]; then
    echo "no registered runner named ${RUNNER_NAME} in ${RUNNER_REPO}"
else
    for runner_id in $runner_ids; do
        gh api -X DELETE "repos/${RUNNER_REPO}/actions/runners/${runner_id}"
        echo "removed runner ${RUNNER_NAME} (${runner_id}) from ${RUNNER_REPO}"
    done
fi

# compose.yml names the project, so the config volume name is stable. The
# .runner/.credentials files are stale once the runner is deleted; drop only
# that volume and keep the mise and build caches for the next registration.
docker volume rm backendrelay-ci-runner_runner-state >/dev/null 2>&1 || true
