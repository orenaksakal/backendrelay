#!/usr/bin/env sh
# Verify the published release images end to end:
#   - each reference is a canonical digest-only image reference
#   - cosign verifies the index digest with the repository's release key
#   - the index publishes both linux/amd64 and linux/arm64
#   - BuildKit attached an SBOM and provenance for both platforms
#
# --shape-only checks only the reference syntax and is safe offline.
set -eu

usage() {
  echo "usage: $0 [--shape-only] IMAGE@sha256:DIGEST [...]" >&2
  exit 2
}

shape_only=false
if [ "${1:-}" = --shape-only ]; then
  shape_only=true
  shift
fi
[ "$#" -gt 0 ] || usage

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

validate_ref() {
  name=$1
  ref=$2
  # Canonical OCI names are lowercase, have at least one registry/repository
  # separator, contain no tag, and end in exactly one sha256 digest.
  if ! printf '%s\n' "$ref" | grep -Eq \
    '^[a-z0-9]+([.-][a-z0-9]+)*(:[0-9]+)?(/[a-z0-9]+([._-][a-z0-9]+)*)+@sha256:[0-9a-f]{64}$'
  then
    echo "$name must be a canonical lowercase digest-only image reference (registry/repository@sha256:<64 lowercase hex>)" >&2
    return 1
  fi
}

# Count literal occurrences in the attestation dumps, which are single-line
# JSON on every current BuildKit release.
count_occurrences() {
  printf '%s' "$2" | grep -Fo "$1" | wc -l
}

for ref in "$@"; do
  validate_ref image "$ref"
done

[ "$shape_only" = false ] || exit 0

verify=${INSTANT_VERIFY_RELEASE_SIGNATURES:-true}
case "$verify" in
  true) ;;
  *) echo 'signature verification is required; use --shape-only only for offline syntax checks' >&2; exit 2 ;;
esac

key=${COSIGN_PUBLIC_KEY:-$script_dir/cosign.pub}
if [ ! -r "$key" ]; then
  echo "trusted cosign public key not found: $key" >&2
  echo 'set COSIGN_PUBLIC_KEY or commit the release public key at self-hosting/cosign.pub' >&2
  exit 1
fi

for command in cosign docker; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "$command is required for release verification" >&2
    exit 1
  fi
done

for ref in "$@"; do
  if ! cosign verify --key "$key" --insecure-ignore-tlog=true "$ref" >/dev/null; then
    echo "cosign could not verify the release signature for $ref" >&2
    exit 1
  fi

  manifest=$(docker buildx imagetools inspect "$ref")
  for platform in linux/amd64 linux/arm64; do
    if ! printf '%s\n' "$manifest" | grep -Eq "Platform:[[:space:]]+${platform}$"; then
      echo "$ref does not publish $platform" >&2
      exit 1
    fi
  done

  sbom=$(docker buildx imagetools inspect "$ref" --format '{{json .SBOM}}')
  if [ "$(count_occurrences 'SPDXRef-DOCUMENT' "$sbom")" -lt 2 ]; then
    echo "$ref is missing an SPDX SBOM attestation for one or both platforms" >&2
    exit 1
  fi

  provenance=$(docker buildx imagetools inspect "$ref" --format '{{json .Provenance}}')
  if [ "$(count_occurrences '"buildType"' "$provenance")" -lt 2 ]; then
    echo "$ref is missing a provenance attestation for one or both platforms" >&2
    exit 1
  fi
done
