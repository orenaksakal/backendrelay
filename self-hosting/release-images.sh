#!/usr/bin/env bash
# Build the signed-release inputs for BackendRelay: multi-architecture server
# and dashboard images with SBOM and provenance attestations.
#
# Without --push the two platforms are built into the buildx cache and the host
# architecture is loaded for local testing. With --push the multi-platform
# indexes are pushed to GHCR and --digests-out writes the manifest digests that
# the release process signs and verifies.
#
# This script is the release build entry point; local and automated release
# builds share the same code path.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: self-hosting/release-images.sh [--push] [--tag <tag>] [--digests-out <file>]

  --push                push both platform indexes to GHCR
  --tag <tag>           release tag (default: the exact git tag, else sha-<12>)
  --digests-out <file>  write the pushed index digests as JSON (requires --push)
EOF
    exit 2
}

push=false
tag=
digests_out=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --push) push=true ;;
        --tag)
            [ "$#" -ge 2 ] || usage
            tag=$2
            shift
            ;;
        --digests-out)
            [ "$#" -ge 2 ] || usage
            digests_out=$2
            shift
            ;;
        *) usage ;;
    esac
    shift
done

if [ -n "$digests_out" ] && [ "$push" != true ]; then
    echo '--digests-out requires --push' >&2
    exit 2
fi

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

revision=$(git rev-parse HEAD)
short_sha=$(git rev-parse --short=12 HEAD)
sha_tag="sha-${short_sha}"
if [ -z "$tag" ]; then
    tag=$(git describe --tags --exact-match 2>/dev/null || printf '%s' "$sha_tag")
fi
case "$tag" in
    [A-Za-z0-9_]* ) ;;
    *) echo "invalid image tag: $tag" >&2; exit 2 ;;
esac
case "$tag" in
    *[!A-Za-z0-9_.-]* ) echo "invalid image tag: $tag" >&2; exit 2 ;;
esac
if [ "${#tag}" -gt 128 ]; then
    echo "invalid image tag: $tag is longer than 128 characters" >&2
    exit 2
fi

# Keep this in sync with the runtime base in server-rs/Dockerfile.
runtime_probe=debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8

case "$(uname -m)" in
    x86_64 | amd64) host_platform=linux/amd64 ;;
    aarch64 | arm64) host_platform=linux/arm64 ;;
    *)
        echo "unsupported build host architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

builder=${BUILDX_BUILDER:-backendrelay-release}
if ! docker buildx inspect "$builder" >/dev/null 2>&1; then
    docker buildx create --name "$builder" --driver docker-container >/dev/null
fi
docker buildx inspect --bootstrap "$builder" >/dev/null

# The runtime stages run apt-get/adduser on the target platform. Building the
# arm64 image on an x64 host therefore needs the kernel's binfmt registration;
# the cross-compiled Rust builder itself does not.
if [ "$host_platform" != linux/arm64 ]; then
    if ! docker run --rm --platform linux/arm64 "$runtime_probe" true >/dev/null 2>&1; then
        cat >&2 <<'EOF'
arm64 emulation is not available on this Docker host.

The target-platform runtime stages need it. Register binfmt once on the host
(root-equivalent Docker access, no image or container is kept):
  docker run --privileged --rm tonistiigi/binfmt --install arm64
EOF
        exit 1
    fi
fi

image_names=(
    ghcr.io/orenaksakal/backendrelay-server
    ghcr.io/orenaksakal/backendrelay-dashboard
)
image_contexts=(server-rs client)
image_dockerfiles=(server-rs/Dockerfile client/dashboard/deploy/Dockerfile)
index_digests=()
image_refs=()

build_image() {
    repository=$1
    context=$2
    dockerfile=$3
    shift 3
    extra_args=("$@")

    tags=(--tag "${repository}:${tag}")
    if [ "$tag" != "$sha_tag" ]; then
        tags+=(--tag "${repository}:${sha_tag}")
    fi

    common_args=(
        --builder "$builder"
        --file "$dockerfile"
        --build-arg "OCI_SOURCE=https://github.com/orenaksakal/backendrelay"
        --build-arg "OCI_REVISION=${revision}"
        "${tags[@]}"
    )
    if [ "${#extra_args[@]}" -gt 0 ]; then
        common_args+=("${extra_args[@]}")
    fi

    if [ "$push" = true ]; then
        docker buildx build \
            "${common_args[@]}" \
            --platform linux/amd64,linux/arm64 \
            --sbom=true \
            --provenance=mode=max \
            --push \
            "$context"
    else
        # Fill the builder cache for both targets, then load only the host
        # architecture into the local image store for a test run.
        docker buildx build \
            "${common_args[@]}" \
            --platform linux/amd64,linux/arm64 \
            "$context"
        docker buildx build \
            "${common_args[@]}" \
            --platform "$host_platform" \
            --load \
            "$context"
    fi
}

for index in "${!image_names[@]}"; do
    repository=${image_names[$index]}
    case "$repository" in
        *backendrelay-server)
            build_image "$repository" "${image_contexts[$index]}" "${image_dockerfiles[$index]}" \
                --build-arg "BACKENDRELAY_VERSION=${tag}"
            ;;
        *)
            build_image "$repository" "${image_contexts[$index]}" "${image_dockerfiles[$index]}"
            ;;
    esac

    if [ "$push" = true ]; then
        inspect=$(docker buildx imagetools inspect "${repository}:${tag}")
        digest=$(printf '%s\n' "$inspect" |
            sed -n 's/^Digest:[[:space:]]*//p' |
            head -n 1)
        case "$digest" in
            sha256:*) ;;
            *)
                echo "could not resolve the pushed index digest for ${repository}:${tag}" >&2
                exit 1
                ;;
        esac
        case "${digest#sha256:}" in
            *[!0-9a-f]* | '') echo "invalid digest for ${repository}:${tag}: $digest" >&2; exit 1 ;;
        esac
        if [ "${#digest}" -ne 71 ]; then
            echo "invalid digest for ${repository}:${tag}: $digest" >&2
            exit 1
        fi
        index_digests+=("$digest")
        image_refs+=("${repository}@${digest}")
    fi
done

if [ "$push" = true ]; then
    if [ -n "$digests_out" ]; then
        {
            printf '{\n'
            printf '  "tag": "%s",\n' "$tag"
            printf '  "revision": "%s",\n' "$revision"
            printf '  "images": [\n'
            for index in "${!image_names[@]}"; do
                if [ "$index" -gt 0 ]; then
                    printf ',\n'
                fi
                printf '    {"name": "%s", "digest": "%s", "ref": "%s"}' \
                    "${image_names[$index]}" "${index_digests[$index]}" "${image_refs[$index]}"
            done
            printf '\n  ]\n'
            printf '}\n'
        } >"$digests_out"
    fi
    printf '%s\n' "${image_refs[@]}"
fi
