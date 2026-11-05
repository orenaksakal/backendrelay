#!/usr/bin/env bash
# Static contracts for the containerized self-hosted CI runner kit.
# No Docker, network, gh, or credentials required.
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
kit="$script_dir/ci-runner"

fail() {
    echo "ci-runner kit contract failed: $*" >&2
    exit 1
}

for file in Dockerfile compose.yml entrypoint.sh register.sh unregister.sh; do
    [[ -f "$kit/$file" ]] || fail "missing $file"
done

for script in "$kit"/*.sh; do
    bash -n "$script" || fail "syntax error in $(basename "$script")"
done

# The base image must stay on the pinned runner release and digest.
grep -Eq '^FROM ghcr\.io/actions/actions-runner:2\.338\.2@sha256:759a1d94a15b05941aba5196974ff43fe15b4cbba29b694b35cbc32a1166b232$' \
    "$kit/Dockerfile" \
    || fail 'Dockerfile does not pin actions-runner 2.338.2 to its sha256 digest'
grep -Fq 'docker-ce-cli' "$kit/Dockerfile" \
    || fail 'Dockerfile does not install docker-ce-cli'
grep -Fq 'docker-compose-plugin' "$kit/Dockerfile" \
    || fail 'Dockerfile does not install docker-compose-plugin'
grep -Fq 'docker-buildx-plugin' "$kit/Dockerfile" \
    || fail 'Dockerfile does not install docker-buildx-plugin'
grep -Fq 'signed-by=/etc/apt/keyrings/docker.asc' "$kit/Dockerfile" \
    || fail 'Dockerfile does not use a signed-by Docker apt keyring'
grep -Fq 'npx -y playwright@1.58.2 install-deps chromium firefox webkit' \
    "$kit/Dockerfile" \
    || fail 'Dockerfile does not install Playwright deps for all three browsers'
grep -Fq 'mise use -g node@24 pnpm@10.34.9' "$kit/Dockerfile" \
    || fail 'Dockerfile does not pre-install Node 24 and pnpm 10.34.9 through mise'
grep -Fq 'sha256sum -c' "$kit/Dockerfile" \
    || fail 'Dockerfile does not verify the cargo-deny archive checksum'
grep -Fq 'COSIGN_SHA256' "$kit/Dockerfile" \
    || fail 'Dockerfile does not pin a cosign checksum'
grep -Fq 'cosign-linux-amd64' "$kit/Dockerfile" \
    || fail 'Dockerfile does not download the cosign linux/amd64 binary'
grep -Fq '/home/runner/.local/bin/cosign' "$kit/Dockerfile" \
    || fail 'Dockerfile does not install cosign on the runner PATH'
if grep -Fq 'ARG COSIGN_SHA256=REPLACE_WITH_SHA256' "$kit/Dockerfile"; then
    fail 'Dockerfile still uses the REPLACE_WITH_SHA256 placeholder; fill the real cosign checksum before rebuilding the runner image'
fi
grep -Eq '^ARG COSIGN_SHA256=[0-9a-f]{64}$' "$kit/Dockerfile" \
    || fail 'Dockerfile COSIGN_SHA256 must be the 64-character lowercase sha256 of the cosign download'
grep -Eq '^USER runner$' "$kit/Dockerfile" \
    || fail 'Dockerfile does not finish as the runner user'

# Compose may mount the socket, the work directory (same path inside), and the
# three named volumes. Nothing else, and never a home directory.
grep -Fq '/var/run/docker.sock:/var/run/docker.sock' "$kit/compose.yml" \
    || fail 'compose.yml does not mount the Docker socket'
if grep -Eq '\$\{?HOME\}' "$kit/compose.yml"; then
    fail 'compose.yml references HOME; the home directory must not be mounted'
fi

mapfile -t mounts < <(
    awk '
        /^    volumes:$/ { inside = 1; next }
        inside && /^      - / { sub(/^      - /, ""); print }
    ' "$kit/compose.yml"
)
expected_mounts=(
    '/var/run/docker.sock:/var/run/docker.sock'
    '"${RUNNER_WORK_DIR}:${RUNNER_WORK_DIR}"'
    'runner-home-cache:/home/runner/.cache'
    'runner-mise-data:/home/runner/.local/share/mise'
    'runner-state:/home/runner/state'
)
[[ ${#mounts[@]} -eq ${#expected_mounts[@]} ]] \
    || fail "compose.yml has ${#mounts[@]} mounts, expected ${#expected_mounts[@]}"
for want in "${expected_mounts[@]}"; do
    grep -Fqx "      - $want" "$kit/compose.yml" \
        || fail "compose.yml is missing the mount: $want"
done
for mount in "${mounts[@]}"; do
    found=0
    for want in "${expected_mounts[@]}"; do
        if [[ "$mount" == "$want" ]]; then
            found=1
            break
        fi
    done
    [[ $found -eq 1 ]] || fail "compose.yml has an unexpected mount: $mount"
done
for volume in runner-home-cache runner-mise-data runner-state; do
    grep -Fqx "  $volume:" "$kit/compose.yml" \
        || fail "compose.yml does not declare the named volume $volume"
done
grep -Fq 'restart: unless-stopped' "$kit/compose.yml" \
    || fail 'compose.yml does not set restart: unless-stopped'
grep -Fq 'no-new-privileges:true' "$kit/compose.yml" \
    || fail 'compose.yml does not set no-new-privileges'
grep -Fq 'driver: local' "$kit/compose.yml" \
    || fail 'compose.yml does not set a local logging driver'
grep -Eq 'max-size:' "$kit/compose.yml" \
    || fail 'compose.yml does not cap the log size'

# The entrypoint receives the registration token through the environment and
# must never echo or write it.
grep -Fq './config.sh' "$kit/entrypoint.sh" \
    || fail 'entrypoint.sh does not run config.sh'
grep -Fq '[[ ! -s .runner || ! -s .credentials ]]' "$kit/entrypoint.sh" \
    || fail 'entrypoint.sh does not guard configuration on .runner/.credentials'
grep -Fq -- '--token "$RUNNER_TOKEN"' "$kit/entrypoint.sh" \
    || fail 'entrypoint.sh does not pass the token to config.sh'
grep -Fq 'exec ./run.sh' "$kit/entrypoint.sh" \
    || fail 'entrypoint.sh does not exec run.sh'
if grep -Eq '(echo|printf)[^#]*\$\{?RUNNER_TOKEN' "$kit/entrypoint.sh"; then
    fail 'entrypoint.sh echoes the registration token'
fi
if grep -Eq '\$\{?RUNNER_TOKEN[^#]*>>?[[:space:]]*[^&]' "$kit/entrypoint.sh"; then
    fail 'entrypoint.sh writes the registration token to a file'
fi
if grep -Fq 'set -x' "$kit/entrypoint.sh"; then
    fail 'entrypoint.sh enables shell tracing'
fi

# register.sh mints a token with gh and forwards it only through the
# environment; it never puts it in an argument or a file.
grep -Fq 'registration-token' "$kit/register.sh" \
    || fail 'register.sh does not mint a registration token'
grep -Fq 'export RUNNER_TOKEN' "$kit/register.sh" \
    || fail 'register.sh does not export RUNNER_TOKEN'
grep -Fq 'docker compose -f "$script_dir/compose.yml" up -d --build' \
    "$kit/register.sh" \
    || fail 'register.sh does not start the compose service'
if grep -Eq 'docker compose[^#]*RUNNER_TOKEN' "$kit/register.sh"; then
    fail 'register.sh passes the token on the docker compose command line'
fi
if grep -Eq '(echo|printf)[^#]*\$\{?RUNNER_TOKEN' "$kit/register.sh"; then
    fail 'register.sh echoes the registration token'
fi
if grep -Eq '\$\{?RUNNER_TOKEN[^#]*>>?[[:space:]]*[^&]' "$kit/register.sh"; then
    fail 'register.sh writes the registration token to a file'
fi
grep -Fq 'RUNNER_WORK_DIR:-$HOME/gh-runner-work' "$kit/register.sh" \
    || fail 'register.sh does not default the work directory to $HOME/gh-runner-work'

grep -Fq 'docker compose -f "$script_dir/compose.yml" down' "$kit/unregister.sh" \
    || fail 'unregister.sh does not stop the compose service'
grep -Fq -- '-X DELETE' "$kit/unregister.sh" \
    || fail 'unregister.sh does not delete the runner through the API'

echo 'Containerized CI runner kit contracts verified without Docker'
