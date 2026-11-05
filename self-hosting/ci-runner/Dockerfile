# GitHub Actions self-hosted runner for this repository.
#
# The owner's desktop has Docker but no passwordless sudo, so the runner is a
# container instead of a dedicated Unix user. Jobs still reach the host Docker
# daemon and bind-mount repository paths, so this image is only safe for
# workflows from this private repository: Docker socket access is root
# equivalent. See ../../.github/RUNNER.md and README.md.
#
# Keep the runner release and digest in sync with compose.yml and RUNNER.md.
# Registration happens in entrypoint.sh on first start; no credential is baked
# into the image.

FROM ghcr.io/actions/actions-runner:2.338.2@sha256:759a1d94a15b05941aba5196974ff43fe15b4cbba29b694b35cbc32a1166b232

ENV DEBIAN_FRONTEND=noninteractive
ENV HOME=/home/runner

USER root

# Packages the workflows use: the C toolchain for the Rust build, the
# PostgreSQL 17/wal2json source build driven by mise (bison, flex, readline,
# ICU, SSL and XML headers), the operator/SDK scripts (python3, jq, age) and
# archive helpers.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        git \
        curl \
        ca-certificates \
        unzip \
        xz-utils \
        jq \
        python3 \
        python3-yaml \
        python3-pip \
        age \
        bison \
        flex \
        libreadline-dev \
        zlib1g-dev \
        libicu-dev \
        libssl-dev \
        libxml2-dev \
        libxslt1-dev \
        uuid-dev \
    && rm -rf /var/lib/apt/lists/*

# Docker CLI, Compose v2 and the buildx plugin from Docker's apt repository, so
# jobs can drive the host daemon through the mounted socket and build
# multi-architecture images.
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
        -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && . /etc/os-release \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${VERSION_CODENAME} stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        docker-ce-cli \
        docker-compose-plugin \
        docker-buildx-plugin \
    && rm -rf /var/lib/apt/lists/*

# mise supplies the Node and pnpm toolchains the workflows call by name. Pin
# the mise release; the tools themselves are pinned with `mise use`.
USER runner
ARG MISE_VERSION=v2026.10.19

ENV PATH="/home/runner/.local/bin:/home/runner/.local/share/mise/shims:${PATH}"

RUN export MISE_VERSION="${MISE_VERSION}"; \
    curl -fsSL https://mise.run | sh \
    && mise --version \
    && mise use -g node@24 pnpm@10.34.9

# The nightly `make audit` lane expects cargo-deny on PATH. Rust itself is
# intentionally absent: the workflows install the project toolchain.
ARG CARGO_DENY_VERSION=0.20.2
# sha256 of the x86_64-unknown-linux-musl release tarball (checked 2026-10-26).
ARG CARGO_DENY_SHA256=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f

RUN set -eu; \
    archive="cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl.tar.gz"; \
    base="https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}"; \
    curl -fsSL -o "/tmp/${archive}" "${base}/${archive}"; \
    echo "${CARGO_DENY_SHA256}  /tmp/${archive}" | sha256sum -c -; \
    mkdir -p /tmp/cargo-deny; \
    tar -xzf "/tmp/${archive}" -C /tmp/cargo-deny; \
    install -m 0755 \
        "$(find /tmp/cargo-deny -type f -name cargo-deny -print -quit)" \
        /home/runner/.local/bin/cargo-deny; \
    rm -rf /tmp/cargo-deny "/tmp/${archive}"; \
    cargo-deny --version

# The release workflow signs and verifies image indexes with cosign. Pin the
# release and its archive checksum; `REPLACE_WITH_SHA256` fails the kit test
# until the owner rebuilds the runner with the real value.
ARG COSIGN_VERSION=v2.5.0
ARG COSIGN_SHA256=REPLACE_WITH_SHA256

RUN set -eu; \
    binary="cosign-linux-amd64"; \
    base="https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}"; \
    curl -fsSL -o "/tmp/${binary}" "${base}/${binary}"; \
    echo "${COSIGN_SHA256}  /tmp/${binary}" | sha256sum -c -; \
    install -m 0755 "/tmp/${binary}" /home/runner/.local/bin/cosign; \
    rm -f "/tmp/${binary}"; \
    cosign version

# Playwright's system libraries for Chromium, Firefox and WebKit. install-deps
# shells out to apt, so run it as root with the mise-installed Node on PATH.
# The browsers themselves are downloaded by the jobs.
USER root

RUN node_bin="$(ls -d /home/runner/.local/share/mise/installs/node/24*/bin | head -n 1)" \
    && HOME=/root PATH="${node_bin}:${PATH}" \
        npx -y playwright@1.58.2 install-deps chromium firefox webkit \
    && rm -rf /root/.npm /var/lib/apt/lists/*

# compose.yml mounts a fresh named volume here; create the mountpoint with the
# runner's ownership so the cache is writable on the first start.
RUN install -d -o runner -g runner /home/runner/.cache \
    && install -d -o runner -g runner -m 0700 /home/runner/state

COPY --chown=runner:runner entrypoint.sh /home/runner/ci-runner/entrypoint.sh

RUN chmod 0755 /home/runner/ci-runner/entrypoint.sh

USER runner
WORKDIR /home/runner

ENTRYPOINT ["/home/runner/ci-runner/entrypoint.sh"]
