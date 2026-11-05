ARG OCI_SOURCE=https://github.com/instantdb/instant
ARG OCI_REVISION=unknown

FROM node:26.12.1-slim@sha256:94816674c62774da0d099a947bf4613b9e1972a2733abd0d0cd80e795f8411d0

ARG OCI_SOURCE
ARG OCI_REVISION
LABEL org.opencontainers.image.source="$OCI_SOURCE"
LABEL org.opencontainers.image.revision="$OCI_REVISION"

ENV NODE_ENV=production
WORKDIR /app

RUN groupadd --system --gid 10001 canary \
    && useradd --system --uid 10001 --gid canary --home-dir /app --no-create-home canary

COPY --chown=10001:10001 package.json ./
COPY --chown=10001:10001 src ./src

USER 10001:10001

EXPOSE 9465

CMD ["node", "src/index.mjs"]
