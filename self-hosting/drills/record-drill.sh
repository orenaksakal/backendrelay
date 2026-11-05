#!/usr/bin/env bash
# Record one measured disaster-recovery drill (P7.8).
#
# The ledger is append-only TSV so the row can be copied by hand into
# docs/operations.md#disaster-recovery-drills. This script never reads Docker,
# the network or a secret; it only validates the measurement and writes a row.
#
# Usage:
#   record-drill.sh --drill a|b|c --date YYYY-MM-DD --seconds N \
#     [--status passed|failed] [--notes TEXT] [--ledger FILE]
#
# Defaults: status=passed, ledger=$DRILL_LEDGER or
# /var/lib/backendrelay/drills/recovery-drills.tsv.
set -Eeuo pipefail
umask 077

usage() {
  cat <<'EOF'
usage: record-drill.sh --drill a|b|c --date YYYY-MM-DD --seconds N [options]

Options:
  --drill a|b|c       Drill identifier: a staging restore, b lifeboat, c one app.
  --date YYYY-MM-DD   UTC date the drill finished.
  --seconds N         Whole seconds from first reading the backup to the last
                      green verification.
  --status STATE      passed (default) or failed.
  --notes TEXT        Short factual note, at most 300 characters. Avoid tabs and
                      vertical bars so the Markdown row stays valid.
  --ledger FILE       Append to FILE instead of DRILL_LEDGER or
                      /var/lib/backendrelay/drills/recovery-drills.tsv.
  -h, --help          Show this help.

The script appends one tab-separated row and prints the matching Markdown row.
Copy it into docs/operations.md#disaster-recovery-drills.
EOF
}

die() {
  printf 'record-drill: %s\n' "$*" >&2
  exit 2
}

drill=
date_value=
seconds=
status=passed
notes=
ledger=${DRILL_LEDGER:-/var/lib/backendrelay/drills/recovery-drills.tsv}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --drill | --date | --seconds | --status | --notes | --ledger)
      [ "$#" -ge 2 ] || die "$1 needs a value"
      case "$1" in
        --drill) drill=$2 ;;
        --date) date_value=$2 ;;
        --seconds) seconds=$2 ;;
        --status) status=$2 ;;
        --notes) notes=$2 ;;
        --ledger) ledger=$2 ;;
      esac
      shift 2
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[ -n "$drill" ] || die "--drill is required"
case "$drill" in
  a | b | c) ;;
  *) die "--drill must be a, b or c" ;;
esac

[ -n "$date_value" ] || die "--date is required"
[[ $date_value =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] ||
  die "--date must be a UTC date in YYYY-MM-DD form"
normalized_date=$(date -u -d "$date_value" +%F 2>/dev/null || true)
[ "$normalized_date" = "$date_value" ] ||
  die "--date is not a real calendar date: $date_value"

[ -n "$seconds" ] || die "--seconds is required"
[[ $seconds =~ ^[0-9]{1,8}$ ]] ||
  die "--seconds must be a whole number of seconds"
# Strip leading zeros so the ledger and the Markdown row agree.
seconds=$((10#$seconds))
[ "$seconds" -gt 0 ] || die "--seconds must be greater than zero"
[ "$seconds" -le 31536000 ] ||
  die "--seconds must be at most one year; check the measurement"

case "$status" in
  passed | failed) ;;
  *) die "--status must be passed or failed" ;;
esac

[[ $notes != *$'\n'* ]] || die "--notes must be one line"
[[ $notes != *'|'* ]] || die "--notes must not contain a vertical bar"
notes=${notes//$'\t'/ }
[ "${#notes}" -le 300 ] || die "--notes must be at most 300 characters"
[ -n "$notes" ] || notes='—'

[ -n "$ledger" ] || die "--ledger must not be empty"
command -v flock >/dev/null 2>&1 || die "flock is required (util-linux)"
# Create only a missing directory: never change the mode of an existing one,
# because --ledger may point at /tmp for a rehearsal.
ledger_dir=$(dirname -- "$ledger")
[ -d "$ledger_dir" ] || mkdir -p -m 700 -- "$ledger_dir"

exec 9>>"$ledger"
flock 9
if [ ! -s "$ledger" ]; then
  printf 'date\tdrill\tseconds\tstatus\tnotes\n' >&9
fi
printf '%s\t%s\t%s\t%s\t%s\n' \
  "$date_value" "$drill" "$seconds" "$status" "$notes" >&9
flock -u 9
exec 9>&-
chmod 600 -- "$ledger"

printf 'recorded drill %s: %s, %s s, %s\n' \
  "$drill" "$date_value" "$seconds" "$status" >&2
printf '| %s | %s | %s s | %s | %s |\n' \
  "$drill" "$date_value" "$seconds" "$status" "$notes"
