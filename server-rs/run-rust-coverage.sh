#!/usr/bin/env bash
set -Eeuo pipefail

server_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$server_root"

readonly expected_llvm_cov_version=0.9.0
readonly ignore_regex='(^|/)(vendor/cel|target|tests|benches)(/|$)|(^|/)build\.rs$'
readonly minimum_lines=84
readonly minimum_functions=77
readonly minimum_regions=82

command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "cargo-llvm-cov $expected_llvm_cov_version is required" >&2
    exit 1
}
actual_version=$(cargo llvm-cov --version | awk '{print $2}')
[[ $actual_version == "$expected_llvm_cov_version" ]] || {
    echo "cargo-llvm-cov $expected_llvm_cov_version is required; found $actual_version" >&2
    exit 1
}

cargo llvm-cov clean --workspace
eval "$(cargo llvm-cov show-env --sh)"
./run-postgres-acceptance.sh

report_args=(
    --workspace
    --ignore-filename-regex "$ignore_regex"
)
cargo llvm-cov report "${report_args[@]}" --summary-only \
    --fail-under-lines "$minimum_lines" \
    --fail-under-functions "$minimum_functions" \
    --fail-under-regions "$minimum_regions"
cargo llvm-cov report "${report_args[@]}" --cobertura \
    --output-path target/coverage.xml
cargo llvm-cov report "${report_args[@]}" --html \
    --output-dir target/coverage-html

echo "Rust coverage gate passed: lines >= $minimum_lines%, functions >= $minimum_functions%, regions >= $minimum_regions%"
