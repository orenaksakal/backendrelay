#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
mkdir -p "$tmp/bin"

cat > "$tmp/bin/docker" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$MOCK_DOCKER_LOG"
case "$*" in
  *' down --volumes --remove-orphans') exit 0 ;;
  *' pull server') exit 0 ;;
  *' build server') exit 0 ;;
  *' pull postgres') exit 0 ;;
  *' build postgres') exit 0 ;;
  *' up -d --no-build postgres s3 createbuckets') exit 42 ;;
  *)
    echo "unexpected docker command: $*" >&2
    exit 99
    ;;
esac
EOF
chmod +x "$tmp/bin/docker"

candidate=ghcr.io/example/instantdb-server@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
candidate_postgres=ghcr.io/example/backendrelay-postgres@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

run_to_fixture_stop() {
  harness=$1
  mode=$2
  log=$tmp/$(basename "$harness").$mode.log
  output=$log.output
  : > "$log"

  if [ "$mode" = candidate ]; then
    if INSTANT_SERVER_IMAGE=$candidate INSTANT_POSTGRES_IMAGE=$candidate_postgres \
      MOCK_DOCKER_LOG=$log \
      PATH="$tmp/bin:$PATH" bash "$harness" >"$output" 2>&1
    then
      status=0
    else
      status=$?
    fi
  else
    if (unset INSTANT_SERVER_IMAGE INSTANT_POSTGRES_IMAGE; MOCK_DOCKER_LOG=$log \
      PATH="$tmp/bin:$PATH" bash "$harness" >"$output" 2>&1)
    then
      status=0
    else
      status=$?
    fi
  fi

  if [ "$status" -ne 42 ]; then
    echo "$(basename "$harness") $mode branch did not reach the mocked Compose stop (status $status)" >&2
    cat "$output" >&2
    exit 1
  fi

  if [ "$mode" = candidate ]; then
    test "$(grep -Fc ' pull server' "$log")" -eq 1
    test "$(grep -Fc ' pull postgres' "$log")" -eq 1
    if grep -Fq ' build server' "$log"; then
      echo "$(basename "$harness") built the server while testing a candidate image" >&2
      exit 1
    fi
    if grep -Fq ' build postgres' "$log"; then
      echo "$(basename "$harness") built PostgreSQL while a pinned image was configured" >&2
      exit 1
    fi
  else
    test "$(grep -Fc ' build server' "$log")" -eq 1
    test "$(grep -Fc ' build postgres' "$log")" -eq 1
    if grep -Fq ' pull server' "$log"; then
      echo "$(basename "$harness") pulled the server in local source-build mode" >&2
      exit 1
    fi
    if grep -Fq ' pull postgres' "$log"; then
      echo "$(basename "$harness") pulled PostgreSQL in local source-build mode" >&2
      exit 1
    fi
  fi
}

reject_mutable_ref() {
  harness=$1
  log=$tmp/$(basename "$harness").invalid.log
  output=$log.output
  : > "$log"

  if INSTANT_SERVER_IMAGE=ghcr.io/example/instantdb-server:latest \
    MOCK_DOCKER_LOG=$log PATH="$tmp/bin:$PATH" \
    bash "$harness" >"$output" 2>&1
  then
    echo "$(basename "$harness") accepted a mutable candidate image reference" >&2
    exit 1
  fi
  grep -Fq 'must be a canonical lowercase digest-only image reference' "$output"
  if [ -s "$log" ]; then
    echo "$(basename "$harness") invoked Docker before rejecting a mutable image reference" >&2
    exit 1
  fi
}

for harness in \
  "$script_dir/run-official-sdk-e2e.sh" \
  "$script_dir/run-official-byop-e2e.sh"
do
  run_to_fixture_stop "$harness" candidate
  run_to_fixture_stop "$harness" local
  reject_mutable_ref "$harness"
done

echo 'Official SDK and BYOP exact-candidate image gating contracts verified without Docker'
