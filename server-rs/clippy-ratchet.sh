#!/usr/bin/env bash
# Fail when the server-rs Clippy warning count grows past the recorded baseline.
#
# This is the minimal ratchet until the tree is warning-free (plan step P5.7).
# The baseline is the number of lines the following command reports:
#   cargo clippy --locked --workspace --all-targets --message-format=short
#
# Usage (from server-rs/):
#   bash clippy-ratchet.sh            # check against clippy-baseline.txt
#   bash clippy-ratchet.sh --update   # lower (or re-record) the baseline
set -Eeuo pipefail

server_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$server_root"

readonly baseline_path="$server_root/clippy-baseline.txt"
readonly warning_pattern='^[^ ].*: warning:'

log=$(mktemp)
trap 'rm -f "$log"' EXIT

echo "Running cargo clippy over the workspace..."
if ! cargo clippy --locked --workspace --all-targets --message-format=short >"$log" 2>&1; then
    cat "$log" >&2
    echo "cargo clippy failed; the ratchet only compares warning counts." >&2
    exit 1
fi

current=$(grep -cE "$warning_pattern" "$log" || true)
baseline=$(tr -d '[:space:]' <"$baseline_path")

if [[ ${1:-} == --update ]]; then
    printf '%s\n' "$current" >"$baseline_path"
    echo "Recorded $current Clippy warnings in $baseline_path."
    exit 0
fi

echo "Clippy warnings: current $current, baseline $baseline"
if ((current > baseline)); then
    echo "Clippy warning count regressed above $baseline_path." >&2
    echo "Fix the new warnings, or rerun with --update only to lower the baseline." >&2
    exit 1
fi

echo "Clippy ratchet passed."
