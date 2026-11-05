#!/usr/bin/env bash
set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
script="$script_dir/run-compose-image-rollback.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/instant-compose-rollback-test.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT HUP INT TERM
mock_bin="$tmp/bin"
mkdir "$mock_bin"

a=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
b=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
candidate="ghcr.io/example/instantdb-server@sha256:$a"
baseline="ghcr.io/example/instantdb-server@sha256:$b"

cat >"$mock_bin/docker" <<'MOCK'
#!/usr/bin/env bash
set -eu
printf '%s|%s\n' "${INSTANT_SERVER_IMAGE:-}" "$*" >>"$MOCK_LOG"
printf '%s\n' "${S3_PUBLIC_ENDPOINT:-}" >>"$MOCK_S3_LOG"

if [[ $1 == inspect ]]; then
  printf '%s\n' instant-compose-rollback-postgres-data
  exit 0
fi

case " $* " in
  *' ps -q postgres '*) printf '%s\n' instant-compose-rollback-postgres ;;
  *' up '*' server '*) printf '%s\n' "$INSTANT_SERVER_IMAGE" >"$MOCK_STATE" ;;
esac
MOCK
chmod +x "$mock_bin/docker"

cat >"$mock_bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -eu
output=
method=GET
url=
data=
has_app_id=false
has_authorization=false
while (($#)); do
  case "$1" in
    --output) output=$2; shift 2 ;;
    -X) method=$2; shift 2 ;;
    --data) data=$2; shift 2 ;;
    -H)
      [[ $2 == 'app-id: '* ]] && has_app_id=true
      [[ $2 == 'authorization: Bearer '* ]] && has_authorization=true
      shift 2
      ;;
    http://*) url=$1; shift ;;
    *) shift ;;
  esac
done
if [[ ${MOCK_FAIL_BASELINE_READ:-0} == 1 ]] && [[ $url == */admin/query ]] && \
    [[ $(cat "$MOCK_STATE") == "$INSTANT_ROLLBACK_SERVER_IMAGE" ]]; then
  exit 22
fi
case "$url" in
  */dash/apps/ephemeral)
    printf '%s\n' '{"app":{"id":"11111111-1111-4111-8111-111111111111","title":"compose-image-rollback-acceptance","admin-token":"admin-secret"}}' >"$output"
    ;;
  */admin/transact)
    [[ $method == POST && $has_app_id == true && $has_authorization == true ]]
    [[ $data == *'"add-attr"'* && $data == *'"add-triple"'* && $data == *'"candidate-data"'* ]]
    printf '%s\n' written >"$MOCK_DATA_STATE"
    printf '%s\n' '{"tx-id":1}' >"$output"
    ;;
  */admin/query)
    [[ $method == POST && $has_app_id == true && $has_authorization == true ]]
    [[ -f $MOCK_DATA_STATE ]]
    printf '%s\n' '{"rollback_checks":[{"id":"66666666-6666-4666-8666-666666666666","marker":"candidate-data"}]}' >"$output"
    ;;
  */dash/apps/ephemeral/*)
    printf '%s\n' '{"app":{"id":"11111111-1111-4111-8111-111111111111","title":"compose-image-rollback-acceptance"}}' >"$output"
    ;;
  *)
    echo "unexpected curl URL: $url" >&2
    exit 2
    ;;
esac
MOCK
chmod +x "$mock_bin/curl"

run_rollback() {
  rm -f -- "$tmp/application-data"
  env PATH="$mock_bin:$PATH" MOCK_LOG="$tmp/docker.log" MOCK_S3_LOG="$tmp/s3.log" \
    MOCK_STATE="$tmp/server.image" MOCK_DATA_STATE="$tmp/application-data" \
    INSTANT_SERVER_IMAGE="$candidate" INSTANT_ROLLBACK_SERVER_IMAGE="$baseline" \
    COMPOSE_ROLLBACK_CONFIRM=replace-candidate-with-baseline-and-restore-candidate \
    COMPOSE_ROLLBACK_READINESS_SECONDS=2 "$@" "$script"
}

: >"$tmp/docker.log"
run_rollback >"$tmp/output"
grep -Fq 'Compose server-image rollback acceptance passed' "$tmp/output"
grep -Fxq 'http://localhost:9000' "$tmp/s3.log"
test "$(cat "$tmp/application-data")" = written
test "$(cat "$tmp/server.image")" = "$candidate"
test "$(grep -Fc "$candidate|" "$tmp/docker.log")" -ge 3
test "$(grep -Fc "$baseline|" "$tmp/docker.log")" -ge 2
test "$(grep -Fc '|inspect --format' "$tmp/docker.log")" -ge 3

: >"$tmp/docker.log"
if run_rollback MOCK_FAIL_BASELINE_READ=1 >"$tmp/output" 2>&1; then
  echo 'incompatible baseline unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'cannot prove baseline compatibility with the candidate-migrated database' "$tmp/output"
grep -Fq 'failure_recovery=candidate_observed' "$tmp/output"
test "$(cat "$tmp/server.image")" = "$candidate"

if run_rollback COMPOSE_ROLLBACK_PROJECT=existing-project >"$tmp/output" 2>&1; then
  echo 'operator-provided Compose project unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'always creates an isolated project' "$tmp/output"

echo 'Compose server-image rollback contract verified without a Docker daemon'
