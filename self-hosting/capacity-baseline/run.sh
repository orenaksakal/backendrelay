#!/usr/bin/env bash
set -Eeuo pipefail

# Runs the P7.9 capacity baseline against an already-deployed staging origin
# (the production hardware and database tier, driven from this machine over the
# public network). The two profiles are fixed by the plan: one app with 200
# users, and twenty apps with ten users each, both through steady (300 s),
# 2x burst (60 s) and recovery (90 s).
#
# Usage:
#   CAPACITY_API_ORIGIN=https://staging-api.example.com \
#     bash self-hosting/capacity-baseline/run.sh single|many|all
#
# Environment overrides: CAPACITY_STATE_DIR, CAPACITY_RUN_TIMEOUT (seconds per
# profile, default 1800), CAPACITY_ALLOW_PRODUCTION=1 to override the api.*
# guard. See README.md in this directory for the staging preparation and for
# recording the results.

capacity_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
harness="$(dirname -- "$capacity_dir")/railway-loadtest"
readonly origin="${CAPACITY_API_ORIGIN:-}"
readonly run_timeout=${CAPACITY_RUN_TIMEOUT:-1800}
readonly min_open_files=4096

usage() {
  cat <<'EOF'
usage: CAPACITY_API_ORIGIN=https://staging-api.example.com run.sh single|many|all

  single  1 app x 200 users
  many    20 apps x 10 users
  all     both profiles, each writing under the same baseline directory

EOF
}

die() {
  echo "$*" >&2
  exit 1
}

case "${1:-}" in
  single|many|all) profile=$1; shift ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

[[ -n "$origin" ]] || die "CAPACITY_API_ORIGIN must name the staging origin (never production)"
authority=${origin#*://}
host=${authority%%/*}
host=${host%%:*}
case "$host" in
  api.*)
    if [[ "${CAPACITY_ALLOW_PRODUCTION:-0}" != 1 ]]; then
      die "refusing origin $origin: it looks like production; the baseline runs against staging (set CAPACITY_ALLOW_PRODUCTION=1 to override)"
    fi
    ;;
esac

for command in node npm timeout; do
  command -v "$command" >/dev/null 2>&1 || die "required command is not installed: $command"
done

if [[ ! -d "$harness/node_modules/@instantdb/core" || ! -d "$harness/node_modules/ws" ]]; then
  echo "==> installing pinned load-generator dependencies"
  (cd -- "$harness" && timeout 600 npm install --no-save --no-audit --no-fund)
fi

# The generator opens one socket per session plus observers; 200 sessions fit
# comfortably in 4096, but a lower soft limit is a silent setup-failure source.
soft=$(ulimit -Sn)
hard=$(ulimit -Hn)
if [[ "$soft" != unlimited ]] && ((soft < min_open_files)); then
  if [[ "$hard" != unlimited ]] && ((hard < min_open_files)); then
    die "open-file hard limit is $hard, below the required $min_open_files"
  fi
  ulimit -S -n "$min_open_files" || die "could not raise the open-file soft limit to $min_open_files"
  echo "==> open-file soft limit raised to $(ulimit -Sn) (hard: $hard)"
fi

state_root="${CAPACITY_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/backendrelay-capacity}"
baseline_dir="$state_root/baseline/$(date -u +%Y-%m-%dT%H-%M-%SZ)"
mkdir -p -- "$baseline_dir"

echo "==> origin $origin"
echo "==> results $baseline_dir"
echo "    run in a quiet window: production shares the host's CPU and disk; read"
echo "    server/PostgreSQL CPU from the host monitoring for each phase window"

profiles=(single many)
[[ "$profile" == all ]] || profiles=("$profile")
declare -A statuses=()
worst=0
for name in "${profiles[@]}"; do
  echo "==> profile $name (limit: ${run_timeout}s)"
  set +e
  timeout --foreground --signal=TERM --kill-after=30s "$run_timeout" \
    node "$harness/remote-scale-run.mjs" --profile "$name" --origin "$origin" \
    --out "$baseline_dir/$name" "$@"
  status=$?
  set -e
  statuses[$name]=$status
  if ((status > worst)); then worst=$status; fi
done

echo
for name in "${profiles[@]}"; do
  case "${statuses[$name]}" in
    0) echo "  $name: PASS" ;;
    1) echo "  $name: FAIL" ;;
    2) echo "  $name: INVALID (measurement rejected; not a capacity result)" ;;
    *) echo "  $name: ERROR (exit ${statuses[$name]})" ;;
  esac
  echo "    $baseline_dir/$name/result.json"
done
echo
echo "Record the phase windows and CPU in docs/performance.md; see"
echo "self-hosting/capacity-baseline/README.md#record-the-results."

exit "$worst"
