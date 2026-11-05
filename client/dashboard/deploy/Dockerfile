ARG OCI_SOURCE=https://github.com/instantdb/instant
ARG OCI_REVISION=unknown

# The dashboard bundle is static, so build it on the host platform and copy it
# into the target-platform Caddy image. This also avoids running pnpm, Node and
# the workspace package builds under arm64 emulation on an amd64 release host.
FROM --platform=$BUILDPLATFORM node:26.12.1-slim AS dependencies

WORKDIR /workspace

# Install the workspace once so turbo prune can resolve every package.
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY patches ./patches
RUN corepack enable pnpm && \
    pnpm install --frozen-lockfile

FROM --platform=$BUILDPLATFORM node:26.12.1-slim AS pruner

WORKDIR /workspace

COPY --from=dependencies /workspace/node_modules ./node_modules
COPY . .
RUN corepack enable pnpm && \
    pnpm turbo prune backendrelay-dashboard --docker

FROM --platform=$BUILDPLATFORM node:26.12.1-slim AS builder

WORKDIR /workspace

COPY --from=pruner /workspace/out/json/ .
COPY --from=pruner /workspace/patches ./patches
RUN corepack enable pnpm && \
    pnpm install --frozen-lockfile

COPY --from=pruner /workspace/out/full/ .
COPY --from=pruner /workspace/tsconfig.base.json ./

# Build the dashboard and the workspace packages it imports. The filter's
# trailing ellipsis includes @instantdb/react, @instantdb/core and the rest in
# dependency order; their package exports point at dist/.
RUN pnpm --filter backendrelay-dashboard... build

FROM caddy:2.11.7-alpine

ARG OCI_SOURCE
ARG OCI_REVISION
LABEL org.opencontainers.image.source="$OCI_SOURCE"
LABEL org.opencontainers.image.revision="$OCI_REVISION"

COPY dashboard/deploy/Caddyfile /etc/caddy/Caddyfile

# SPA mode writes one shell (`_shell.html`) plus hashed assets to dist/client.
# `dist/server` is build-time only and is deliberately not copied.
COPY --from=builder /workspace/dashboard/dist/client /srv

# Caddy keeps autosave state in /config and runtime state in /data; running as an
# unprivileged numeric user keeps both writable without a passwd entry.
ENV XDG_CONFIG_HOME=/config
ENV XDG_DATA_HOME=/data
RUN mkdir -p /config /data /srv && chown -R 10001:10001 /config /data /srv
USER 10001:10001

EXPOSE 8080
