ARG OCI_SOURCE=https://github.com/instantdb/instant

FROM postgres:18.4-bookworm@sha256:882236b897e39051d2368c5ccc6cda944904723506b2dfc97f2a8f5bc9afa382

ARG OCI_SOURCE
LABEL org.opencontainers.image.source="$OCI_SOURCE"
LABEL org.opencontainers.image.description="PostgreSQL 17 with wal2json, pg_hint_plan, and pgBackRest for BackendRelay"
LABEL org.opencontainers.image.licenses="PostgreSQL"

# PGDG publishes these packages for amd64 and arm64, so the image needs no
# compiler. Bump the base digest and all three package versions together; see
# README.md.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      postgresql-17-wal2json=2.6-4.pgdg12+1 \
      postgresql-17-pg-hint-plan=1.7.1-1.pgdg12+1 \
      pgbackrest=2.59.3-1.pgdg12+1 \
 && rm -rf /var/lib/apt/lists/*

# The named pgBackRest spool volume is created root-owned; fix it before the
# stock entrypoint drops to the postgres user.
COPY entrypoint.sh /usr/local/bin/backendrelay-postgres-entrypoint.sh
RUN chmod 0755 /usr/local/bin/backendrelay-postgres-entrypoint.sh

# Runs only on a fresh data directory and creates the unprivileged application
# role from INSTANT_DB_USER and INSTANT_DB_PASSWORD.
COPY --chmod=0755 initdb/10-backendrelay-role.sh /docker-entrypoint-initdb.d/10-backendrelay-role.sh
ENTRYPOINT ["/usr/local/bin/backendrelay-postgres-entrypoint.sh"]
# Setting ENTRYPOINT resets the CMD inherited from the base image; restore it.
CMD ["postgres"]
