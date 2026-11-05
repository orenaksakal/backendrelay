#!/usr/bin/env bash
# Gate a release tag with evidence a single operator can gather:
#   tag -> release-images.yml + nightly.yml -> make release-check TAG=<tag> -> deploy
#
# The procedure is documented in docs/operations.md. Every check prints
# `check=<name> result=pass|fail`; the script exits non-zero if any check fails.
# The first release has no previous tag, so the upgrade/rollback check is
# skipped with a clear message instead of failing.
#
# Requires git, an authenticated `gh`, jq, docker, and cosign.
set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root"

usage() {
  cat <<'EOF'
usage: self-hosting/release-check.sh <tag>

Verify that <tag> is ready to deploy:
  1. the tag exists and the latest nightly.yml run for its commit succeeded
  2. release-images.yml published signed digests for that commit, and the
     digests verify (platforms, cosign signature, SBOM and provenance)
  3. the previous release upgrades and rolls back to this one
  4. the official SDK smoke suite passes against the release server digest
  5. CHANGELOG.md has a section for the tag

Requires git, gh (authenticated), jq, docker and cosign. COSIGN_PUBLIC_KEY
overrides the default self-hosting/cosign.pub trust anchor.
EOF
}

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
  '')
    usage >&2
    exit 2
    ;;
esac
if [ "$#" -ne 1 ]; then
  usage >&2
  exit 2
fi

tag=$1
commit=
tmp=$(mktemp -d "${TMPDIR:-/tmp}/backendrelay-release-check.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT HUP INT TERM

names=()
results=()
record() {
  names+=("$1")
  results+=("$2")
  if [ -n "${3:-}" ]; then
    printf 'check=%s result=%s detail=%s\n' "$1" "$2" "$3"
  else
    printf 'check=%s result=%s\n' "$1" "$2"
  fi
}

finish() {
  local overall=$1 index
  printf '%s\n' '--- release-check summary ---'
  for index in "${!names[@]}"; do
    printf 'check=%s result=%s\n' "${names[$index]}" "${results[$index]}"
  done
  printf 'release-check result=%s tag=%s commit=%s\n' \
    "$overall" "$tag" "${commit:-unknown}"
  [ "$overall" = pass ]
}

fail() {
  local name=$1
  shift
  record "$name" fail "$*"
  finish fail || true
  exit 1
}

for command in git gh jq docker cosign; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "release-check: required command is not installed: $command" >&2
    exit 2
  }
done

# 1. The tag exists and its nightly run is green.
if ! commit=$(git rev-parse --verify --quiet "refs/tags/${tag}^{commit}"); then
  fail tag "tag does not exist: $tag"
fi
head=$(git rev-parse HEAD)
record tag pass "commit=$commit head=$head"
if [ "$head" != "$commit" ]; then
  echo "release-check: warning: HEAD $head is not the tagged commit $commit; checking the tag from this checkout" >&2
fi

if ! nightly_conclusion=$(gh run list --workflow nightly.yml --commit "$commit" \
    --limit 1 --json conclusion --jq '.[0].conclusion // empty' 2>/dev/null); then
  fail nightly "could not list nightly.yml runs for $commit"
fi
if [ "$nightly_conclusion" != success ]; then
  fail nightly "latest nightly.yml run for $commit concluded ${nightly_conclusion:-no run}"
fi
record nightly pass "conclusion=success"

# 2. The release workflow published digests for this commit and they verify.
if ! release_run=$(gh run list --workflow release-images.yml --commit "$commit" \
    --limit 1 --json databaseId,conclusion 2>/dev/null); then
  fail release-images "could not list release-images.yml runs for $commit"
fi
release_run_id=$(jq -r '.[0].databaseId // empty' <<<"$release_run")
release_conclusion=$(jq -r '.[0].conclusion // empty' <<<"$release_run")
if [ -z "$release_run_id" ]; then
  fail release-images "no release-images.yml run for $commit"
fi
if [ "$release_conclusion" != success ]; then
  fail release-images "release-images.yml run $release_run_id concluded ${release_conclusion:-unknown}"
fi

digests_dir="$tmp/digests"
mkdir -p "$digests_dir"
if ! gh run download "$release_run_id" --name "release-digests-$commit" \
    --dir "$digests_dir" >/dev/null 2>&1; then
  fail release-images "run $release_run_id has no release-digests-$commit artifact"
fi
digests=$(find "$digests_dir" -type f -name release-digests.json -print -quit)
if [ -z "$digests" ] || [ ! -s "$digests" ]; then
  fail release-images "run $release_run_id returned no release-digests.json"
fi
if ! jq -e --arg tag "$tag" --arg revision "$commit" \
    '(.tag == $tag) and (.revision == $revision) and ((.images | length) == 2)' \
    "$digests" >/dev/null; then
  fail release-images "release-digests.json does not describe tag=$tag revision=$commit with two images"
fi
mapfile -t image_refs < <(jq -r '.images[].ref' "$digests")
if ! server_ref=$(jq -r \
    '.images[] | select(.name | endswith("backendrelay-server")) | .ref' \
    "$digests"); then
  fail release-images "could not read the server image from release-digests.json"
fi
if [ -z "$server_ref" ]; then
  fail release-images "release-digests.json has no server image"
fi
if ! COSIGN_PUBLIC_KEY="${COSIGN_PUBLIC_KEY:-$script_dir/cosign.pub}" \
    bash "$script_dir/verify-release-images.sh" "${image_refs[@]}"; then
  fail release-images "signature, platform or attestation verification failed"
fi
record release-images pass "run=$release_run_id images=${#image_refs[@]} server=$server_ref"

# 3. The previous release upgrades and rolls back to this digest.
previous_tag=
while IFS= read -r candidate; do
  if [ -n "$candidate" ] && [ "$candidate" != "$tag" ]; then
    previous_tag=$candidate
    break
  fi
done < <(git tag --list 'v*' --sort=-v:refname --merged "$commit")

if [ -z "$previous_tag" ]; then
  record upgrade-rollback skip 'no previous v* release to upgrade from'
else
  previous_commit=$(git rev-parse "refs/tags/${previous_tag}^{commit}")
  if ! previous_run=$(gh run list --workflow release-images.yml --commit "$previous_commit" \
      --limit 1 --json databaseId,conclusion 2>/dev/null); then
    fail upgrade-rollback "could not list release-images.yml runs for previous release $previous_tag"
  fi
  previous_run_id=$(jq -r '.[0].databaseId // empty' <<<"$previous_run")
  previous_conclusion=$(jq -r '.[0].conclusion // empty' <<<"$previous_run")
  if [ -z "$previous_run_id" ]; then
    fail upgrade-rollback "previous release $previous_tag has no release-images.yml run"
  fi
  if [ "$previous_conclusion" != success ]; then
    fail upgrade-rollback "previous release $previous_tag has no successful release-images.yml run"
  fi

  previous_dir="$tmp/previous-digests"
  mkdir -p "$previous_dir"
  if ! gh run download "$previous_run_id" --name "release-digests-$previous_commit" \
      --dir "$previous_dir" >/dev/null 2>&1; then
    fail upgrade-rollback "previous release $previous_tag has no release-digests-$previous_commit artifact"
  fi
  previous_digests=$(find "$previous_dir" -type f -name release-digests.json -print -quit)
  if [ -z "$previous_digests" ]; then
    fail upgrade-rollback "previous release $previous_tag returned no release-digests.json"
  fi
  if ! previous_server_ref=$(jq -r \
      '.images[] | select(.name | endswith("backendrelay-server")) | .ref' \
      "$previous_digests"); then
    fail upgrade-rollback "could not read the previous server image from release-digests.json"
  fi
  if [ -z "$previous_server_ref" ]; then
    fail upgrade-rollback "previous release $previous_tag has no server image digest"
  fi
  if ! bash "$script_dir/run-upgrade-rollback-test.sh" \
      --from-image "$previous_server_ref" --to-image "$server_ref"; then
    fail upgrade-rollback "upgrade and rollback from $previous_tag to $tag failed"
  fi
  record upgrade-rollback pass "from=$previous_tag to=$tag"
fi

# 4. The SDK smoke suite passes against the exact release digest.
if ! INSTANT_SERVER_IMAGE="$server_ref" \
    bash "$script_dir/run-official-sdk-e2e.sh" --suite smoke; then
  fail sdk-smoke "official SDK smoke failed against $server_ref"
fi
record sdk-smoke pass "image=$server_ref"

# 5. The changelog has the release section.
if ! grep -Fq "## [$tag]" CHANGELOG.md; then
  fail changelog "CHANGELOG.md has no '## [$tag]' section"
fi
record changelog pass

finish pass
