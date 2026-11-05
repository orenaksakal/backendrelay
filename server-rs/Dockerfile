ARG OCI_SOURCE=https://github.com/orenaksakal/backendrelay
ARG OCI_REVISION=unknown

# Build on the host platform and cross-compile the target binary, so a
# multi-architecture build needs no arm64 builder. The runtime stage stays on
# the target platform.
FROM --platform=$BUILDPLATFORM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder

ARG TARGETARCH
ARG OCI_REVISION=unknown
ARG BACKENDRELAY_VERSION

WORKDIR /build

# Embed the release tag and source revision reported by `--version` and
# `GET /health/version`. An empty BACKENDRELAY_VERSION falls back to the crate
# version; pass OCI_REVISION to embed the source revision.
ENV BACKENDRELAY_VERSION=${BACKENDRELAY_VERSION}
ENV BACKENDRELAY_GIT_SHA=${OCI_REVISION}

# aws-lc-sys and ring compile C from source, so the arm64 build needs the target
# C compiler and linker. The variables are inert for the amd64 build and for a
# plain `docker build` on the host.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
ENV CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
ENV AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc-ar

RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake perl \
    && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) \
        ;; \
      arm64) \
        apt-get update; \
        apt-get install --yes --no-install-recommends \
          gcc-aarch64-linux-gnu libc6-dev-arm64-cross; \
        rm -rf /var/lib/apt/lists/*; \
        rustup target add aarch64-unknown-linux-gnu \
        ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac

COPY . .

RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) triple=x86_64-unknown-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    cargo build --locked --release -p instant-server --no-default-features --target "$triple"; \
    install -D -m 0755 "target/${triple}/release/instant-server" /build/instant-server

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8 AS runtime

ARG OCI_SOURCE
ARG OCI_REVISION
LABEL org.opencontainers.image.source="$OCI_SOURCE"
LABEL org.opencontainers.image.revision="$OCI_REVISION"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 instant \
    && install -d -o instant -g instant -m 700 /home/instant/config

COPY --from=builder --chmod=0755 /build/instant-server /usr/local/bin/instant-server
COPY --chmod=0755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

USER instant
WORKDIR /home/instant

EXPOSE 8888

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
