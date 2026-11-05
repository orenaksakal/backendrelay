#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
harness="$script_dir/run-dashboard-smoke.sh"

bash -n "$harness"

grep -Fq 'docker-compose.test.yml' "$harness"
grep -Fq 'sockets = [socket.socket() for _ in range(4)]' "$harness"
grep -Fq 'down --volumes --remove-orphans' "$harness"
grep -Fq 'logs --tail 200 postgres server www' "$harness"
grep -Fq 'publicSelfHostedVariables.js' "$harness"
grep -Fq 'Enter your email address' "$harness"
grep -Fq 'reserveStreamFastPath' "$harness"

for forbidden in playwright Playwright; do
  if grep -Fq "$forbidden" "$harness"; then
    echo "dashboard smoke harness must not depend on $forbidden" >&2
    exit 1
  fi
done

echo 'Dashboard runtime smoke contract verified without Docker'
