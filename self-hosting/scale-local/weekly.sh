#!/usr/bin/env bash
set -Eeuo pipefail

# Runs the weekly scale regression: two scale-local shapes on a disposable,
# CPU-pinned stack, keeps each run's result.json under the weekly results
# directory, and compares them with the committed baseline. The systemd user
# timer in systemd/ schedules it, and it should also be run before a release
# that touches the realtime or query path; see README.md.
#
# Usage:
#   weekly.sh [--no-compare]
#
# Environment overrides:
#   WEEKLY_OUT (results root), WEEKLY_STATE_DIR (disposable stack state),
#   WEEKLY_STEADY / WEEKLY_BURST / WEEKLY_RECOVERY (seconds per phase),
#   WEEKLY_RAMP_PER_SECOND (project-wide session ramp), WEEKLY_STACK_TIMEOUT.

scale_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly state_root="${XDG_STATE_HOME:-$HOME/.local/state}/instant-scale-local"
readonly results_root="${WEEKLY_OUT:-$state_root/weekly}"
# The scheduled run owns its own stack so it can never tear down an interactive
# scale-local stack recorded in the default state directory.
export SCALE_STATE_DIR="${WEEKLY_STATE_DIR:-$state_root/weekly-state}"
readonly state_dir=$SCALE_STATE_DIR
readonly steady=${WEEKLY_STEADY:-120}
readonly burst=${WEEKLY_BURST:-30}
readonly recovery=${WEEKLY_RECOVERY:-30}
readonly ramp_per_second=${WEEKLY_RAMP_PER_SECOND:-100}
readonly stack_timeout=${WEEKLY_STACK_TIMEOUT:-2700}
# name apps users-per-app: 10 apps x 1,000 users and 50 apps x 100 users.
readonly scenarios=('10x1000 10 1000' '50x100 50 100')

usage() {
  cat <<'EOF'
usage: weekly.sh [--no-compare]

  Runs both weekly scale-local scenarios on a stack owned by this script,
  copies each result.json to $WEEKLY_OUT/<UTC date>/<scenario>.json, and
  compares them with baseline.json. Exits non-zero when a run failed or a
  comparison regressed. --no-compare keeps the results without comparing.

  The run takes roughly 15 minutes on the desktop. Phase lengths are shortened
  from the defaults so the two scenarios fit the weekly window; override them
  with WEEKLY_STEADY, WEEKLY_BURST and WEEKLY_RECOVERY (seconds).
EOF
}

compare=true
case "${1:-}" in
  --no-compare) compare=false ;;
  -h | --help) usage; exit 0 ;;
  '') ;;
  *)
    usage >&2
    exit 2
    ;;
esac

day=$(date -u +%Y-%m-%d)
out_dir="$results_root/$day"
mkdir -p -- "$out_dir"

# The stack state belongs to weekly.sh, so an interrupted run may leave one
# behind; stop it (which also removes its data directory) before provisioning.
if [[ -e "$state_dir/stack.json" ]]; then
  echo "==> stopping the stack left by an earlier weekly run"
  timeout 300 bash "$scale_dir/stack.sh" down
fi

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  timeout 300 bash "$scale_dir/stack.sh" down || true
  exit "$status"
}
trap cleanup EXIT INT TERM

echo "==> provisioning the weekly scale stack"
timeout "$stack_timeout" bash "$scale_dir/stack.sh" up

run_failed=0
for scenario in "${scenarios[@]}"; do
  read -r name apps users <<<"$scenario"
  run_dir="$state_dir/runs/weekly-$name"
  echo "==> scenario $name: $apps apps x $users users (steady ${steady}s, burst ${burst}s, recovery ${recovery}s)"
  status=0
  bash "$scale_dir/run.sh" --apps "$apps" --users-per-app "$users" \
    --steady "$steady" --burst "$burst" --recovery "$recovery" \
    --ramp-per-second "$ramp_per_second" --out "$run_dir" || status=$?
  if [[ ! -f "$run_dir/result.json" ]]; then
    echo "scenario $name produced no result.json (run exited $status)" >&2
    run_failed=1
    continue
  fi
  cp -- "$run_dir/result.json" "$out_dir/$name.json"
  echo "==> kept $out_dir/$name.json"
  if ((status != 0)); then
    echo "scenario $name exited $status; the comparison reports what it measured" >&2
    run_failed=1
  fi
done

failed=$run_failed
if [[ "$compare" == true ]]; then
  python3 "$scale_dir/weekly_compare.py" --results-dir "$out_dir" || failed=1
else
  echo "==> comparison skipped (--no-compare); results are in $out_dir"
fi
exit "$failed"
