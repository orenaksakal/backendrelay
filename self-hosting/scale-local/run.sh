#!/usr/bin/env bash
set -Eeuo pipefail

# Drives the local multi-app load runner against the disposable stack from
# self-hosting/scale-local/stack.sh. Brings the stack up when it is not already
# running (--fresh forces a clean down/up), installs the pinned load-generator
# dependencies on first use, and runs the generator on the stack's generator
# CPUs under a bounded timeout. The stack is deliberately left running so a
# follow-up tier can reuse it; stop it with stack.sh down.
#
# Usage:
#   run.sh [--fresh] [runner args...]
#
# Runner args: --apps, --users-per-app, --steady, --burst, --recovery,
#   --ramp-per-second, --out, --stack. Environment overrides:
#   SCALE_STATE_DIR, SCALE_STACK_TIMEOUT, SCALE_NPM_TIMEOUT,
#   SCALE_RUN_TIMEOUT (seconds; derived from the phase lengths by default).

scale_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
harness="$(dirname -- "$scale_dir")/railway-loadtest"
readonly state_dir="${SCALE_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/instant-scale-local/current}"
stack_json="$state_dir/stack.json"
readonly stack_timeout=${SCALE_STACK_TIMEOUT:-2700}
readonly npm_timeout=${SCALE_NPM_TIMEOUT:-600}
readonly min_open_files=65536

fresh=false
args=()
while (($#)); do
  case "$1" in
    --fresh) fresh=true; shift ;;
    *) args+=("$1"); shift ;;
  esac
done

# An explicit --stack wins over the default state directory.
for ((index = 0; index + 1 < ${#args[@]}; index += 1)); do
  if [[ "${args[index]}" == --stack ]]; then
    stack_json=${args[index + 1]}
  fi
done

stack_state() {
  python3 - "$stack_json" <<'PY'
import json
import os
import sys

try:
    stack = json.load(open(sys.argv[1]))
    alive = [
        isinstance(stack.get(key), int) and os.path.exists(f"/proc/{stack[key]}")
        for key in ("server_pid", "postmaster_pid")
    ]
except (OSError, ValueError):
    print("missing")
else:
    print("alive" if all(alive) else "stale")
PY
}

for command in python3 node npm taskset timeout; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done

if [[ "$fresh" == true ]]; then
  echo "==> --fresh: stopping any recorded stack"
  timeout 300 bash "$scale_dir/stack.sh" down || true
fi

case "$(stack_state)" in
  alive) echo "==> reusing stack at $stack_json" ;;
  stale)
    echo "==> stopping stale stack at $stack_json"
    timeout 300 bash "$scale_dir/stack.sh" down || true
    timeout "$stack_timeout" bash "$scale_dir/stack.sh" up
    ;;
  *)
    timeout "$stack_timeout" bash "$scale_dir/stack.sh" up
    ;;
esac

if [[ ! -d "$harness/node_modules/@instantdb/core" || ! -d "$harness/node_modules/ws" ]]; then
  echo "==> installing pinned load-generator dependencies"
  (cd -- "$harness" && timeout "$npm_timeout" npm install --no-save --no-audit --no-fund)
fi

# The generator opens one socket per session plus observers, so its soft limit
# must match the ceiling stack.sh applies to the server and Postgres.
soft=$(ulimit -Sn)
hard=$(ulimit -Hn)
if [[ "$soft" != unlimited ]] && ((soft < min_open_files)); then
  if [[ "$hard" != unlimited ]] && ((hard < min_open_files)); then
    echo "open-file hard limit is $hard, below the required $min_open_files" >&2
    exit 1
  fi
  ulimit -S -n "$min_open_files" || {
    echo "could not raise the open-file soft limit to $min_open_files" >&2
    exit 1
  }
  echo "==> open-file soft limit raised to $(ulimit -Sn) (hard: $hard)"
fi

generator_cpus=$(python3 - "$stack_json" <<'PY'
import json
import sys

print(json.load(open(sys.argv[1]))["generator_cpus"])
PY
)
if [[ -z "$generator_cpus" ]]; then
  echo "the stack reserved no generator CPUs; set SCALE_SUT_CPUS/SCALE_SUT_PHYSICAL_CORES and retry" >&2
  exit 1
fi

# Bound the run from the phase arithmetic: the ramp for a project-wide session
# target of apps*users-per-app at --ramp-per-second plus every hold, the
# per-phase drain for in-flight operations, and setup/seeding slack.
read -r scenario_seconds < <(python3 - "${args[@]+"${args[@]}"}" <<'PY'
import math
import sys

values = {"apps": 10, "users-per-app": 1000, "steady": 300, "burst": 60,
          "recovery": 90, "ramp-per-second": 50}
args = sys.argv[1:]
for index in range(0, len(args) - 1, 2):
    if args[index].startswith("--"):
        values[args[index][2:]] = args[index + 1]
total = float(values["apps"]) * float(values["users-per-app"])
ramp = max(30, math.ceil(total / float(values["ramp-per-second"])))
phases = ramp + 15 + 15
holds = float(values["steady"]) + float(values["burst"]) + float(values["recovery"])
print(math.ceil(phases + holds + 3 * 12 + 300))
PY
)
run_timeout=${SCALE_RUN_TIMEOUT:-${scenario_seconds:-3600}}

echo "==> generator CPUs $generator_cpus (limit: ${run_timeout}s)"
set +e
timeout --foreground --signal=TERM --kill-after=30s "$run_timeout" \
  taskset -c "$generator_cpus" node "$harness/local-scale-run.mjs" \
  ${args[@]+"${args[@]}"} --stack "$stack_json"
status=$?
set -e
if ((status != 0)); then
  echo "run failed with exit $status; the stack is left running for inspection" >&2
fi
exit "$status"
