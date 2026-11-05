#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_REPO_URL:?RUNNER_REPO_URL is required}"
: "${RUNNER_WORK_DIR:?RUNNER_WORK_DIR is required}"
RUNNER_NAME="${RUNNER_NAME:-framework-desktop}"

# The runner installation stays where the base image puts it (/home/runner), so
# rebuilding the image upgrades it. Only the registration state persists, in
# the runner-state volume.
cd /home/runner
state=/home/runner/state
mkdir -p "$RUNNER_WORK_DIR"
for file in .runner .credentials .credentials_rsaparams; do
    if [[ -s "$state/$file" ]]; then
        cp "$state/$file" "./$file"
    fi
done

# The registration token is only needed while configuring. Never echo it and
# never persist it: config.sh writes long-lived credentials, which are copied
# into the state volume so restarts never need a new token.
if [[ ! -s .runner || ! -s .credentials ]]; then
    if [[ -z "${RUNNER_TOKEN:-}" ]]; then
        echo "RUNNER_TOKEN is required for the first start" >&2
        exit 1
    fi

    ./config.sh \
        --unattended \
        --url "$RUNNER_REPO_URL" \
        --token "$RUNNER_TOKEN" \
        --name "$RUNNER_NAME" \
        --labels backendrelay \
        --work "$RUNNER_WORK_DIR" \
        --replace \
        --disableupdate

    for file in .runner .credentials .credentials_rsaparams; do
        if [[ -f "$file" ]]; then
            install -m 0600 "$file" "$state/$file"
        fi
    done
fi
unset RUNNER_TOKEN

exec ./run.sh
