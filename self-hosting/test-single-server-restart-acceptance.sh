#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
harness="$script_dir/run-single-server-restart-acceptance.sh"
probe="$script_dir/single-server-restart-acceptance.mjs"

bash -n "$harness"
node --check "$probe"

grep -Fq 'restart --timeout 10 server' "$probe"
grep -Fq '../client/packages/admin/dist/esm/index.js' "$probe"
grep -Fq 'db.subscribeQuery' "$probe"
grep -Fq 'db.query' "$probe"
grep -Fq 'composeContainerId("postgres")' "$probe"
test "$(grep -Fc 'db.transact' "$probe")" -ge 2

grep -Fq 'down --volumes --remove-orphans' "$harness"
grep -Fq 'INSTANT_SERVER_PORT' "$harness"
grep -Fq 'INSTANT_POSTGRES_PORT' "$harness"
grep -Fq 'INSTANT_S3_TEST_PORT' "$harness"
grep -Fq 'listeners = [socket.socket() for _ in range(3)]' "$harness"
grep -Fq 'Acceptance ports must be distinct.' "$harness"
grep -Fq 'export COMPOSE_PROJECT_NAME=instant-single-server-restart-$$-$RANDOM' "$harness"

echo 'Single-server restart acceptance contract verified'
