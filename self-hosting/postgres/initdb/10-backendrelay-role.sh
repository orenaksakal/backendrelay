#!/bin/sh
# Create the unprivileged application role. The stock entrypoint runs this
# directory only while initializing an empty data directory, so an existing
# volume keeps its current roles; see docs/self-hosting.md for the one-time
# conversion SQL.
set -eu

: "${POSTGRES_DB:?POSTGRES_DB must be set}"
: "${INSTANT_DB_USER:?INSTANT_DB_USER must be set}"
: "${INSTANT_DB_PASSWORD:?INSTANT_DB_PASSWORD must be set}"

psql -v ON_ERROR_STOP=1 \
  --username "$POSTGRES_USER" \
  --dbname "$POSTGRES_DB" \
  --set=db="$POSTGRES_DB" \
  --set=role="$INSTANT_DB_USER" \
  --set=password="$INSTANT_DB_PASSWORD" <<'SQL'
SELECT format('CREATE ROLE %I LOGIN REPLICATION NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD %L', :'role', :'password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'role') \gexec
ALTER DATABASE :"db" OWNER TO :"role";
GRANT CREATE ON DATABASE :"db" TO :"role";
ALTER SCHEMA public OWNER TO :"role";
-- The exporter reads pg_stat_statements for statement totals. Preloading the
-- library is a server setting; this per-database extension creates the view.
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
SQL
