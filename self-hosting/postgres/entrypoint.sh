#!/bin/sh
# Docker creates named volumes as root, but PostgreSQL and pgBackRest run as the
# postgres user. Make the async archive spool writable before the stock entrypoint
# drops privileges. The directory is absent in stacks that do not archive WAL.
set -e
if [ "$(id -u)" = "0" ] && [ -d /var/spool/pgbackrest ]; then
  chown postgres:postgres /var/spool/pgbackrest
  chmod 0700 /var/spool/pgbackrest
fi
exec docker-entrypoint.sh "$@"
