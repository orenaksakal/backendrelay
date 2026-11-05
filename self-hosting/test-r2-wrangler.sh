#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

fake_wrangler="$tmp_dir/wrangler"
cat >"$fake_wrangler" <<'EOF'
#!/bin/sh
set -eu
if [ "${R2_ACCESS_KEY_ID+x}" = x ] || [ "${R2_SECRET_ACCESS_KEY+x}" = x ] || \
   [ "${AWS_ACCESS_KEY_ID+x}" = x ] || [ "${INSTANT_SELF_HOST_ENV_FILE+x}" = x ]; then
  echo "static credential environment reached Wrangler" >&2
  exit 1
fi
printf '%s\n' "$*" >>"$FAKE_R2_LOG"

case "$1 ${2:-} ${3:-} ${4:-}" in
  "whoami   ") exit 0 ;;
  "r2 bucket info "*)
    test -f "$FAKE_R2_STATE/bucket-$4"
    printf '{"name":"%s"}\n' "$4"
    ;;
  "r2 bucket create "*)
    : >"$FAKE_R2_STATE/bucket-$4"
    ;;
  "r2 bucket cors set")
    bucket=$5
    shift 5
    while [ "$#" -gt 0 ]; do
      if [ "$1" = "--file" ]; then
        cp "$2" "$FAKE_R2_STATE/cors-$bucket.json"
        break
      fi
      shift
    done
    ;;
  "r2 bucket cors list")
    test -f "$FAKE_R2_STATE/cors-$5.json"
    ;;
  "r2 bucket lifecycle set")
    bucket=$5
    shift 5
    while [ "$#" -gt 0 ]; do
      if [ "$1" = "--file" ]; then
        cp "$2" "$FAKE_R2_STATE/lifecycle-$bucket.json"
        break
      fi
      shift
    done
    ;;
  "r2 bucket lifecycle list")
    test -f "$FAKE_R2_STATE/lifecycle-$5.json"
    ;;
  "r2 object put "*)
    object=$4
    shift 4
    source=
    while [ "$#" -gt 0 ]; do
      if [ "$1" = "--file" ]; then source=$2; break; fi
      shift
    done
    destination="$FAKE_R2_STATE/objects/$object"
    mkdir -p "$(dirname "$destination")"
    cp "$source" "$destination"
    ;;
  "r2 object get "*)
    object=$4
    shift 4
    destination=
    while [ "$#" -gt 0 ]; do
      if [ "$1" = "--file" ]; then destination=$2; break; fi
      shift
    done
    cp "$FAKE_R2_STATE/objects/$object" "$destination"
    ;;
  "r2 object delete "*)
    rm -f "$FAKE_R2_STATE/objects/$4"
    ;;
  *) echo "unexpected fake Wrangler invocation: $*" >&2; exit 1 ;;
esac
EOF
chmod +x "$fake_wrangler"

state="$tmp_dir/state"
mkdir -p "$state"
env_file="$tmp_dir/instant.env"
cat >"$env_file" <<'EOF'
R2_FILES_BUCKET=instant-test-files
R2_BACKUPS_BUCKET='instant-test-backups'
R2_ACCESS_KEY_ID=must-not-be-read
R2_SECRET_ACCESS_KEY=must-not-be-exported
EOF

run_r2() {
  FAKE_R2_STATE="$state" FAKE_R2_LOG="$tmp_dir/wrangler.log" \
    R2_FILES_BUCKET=instant-test-files R2_BACKUPS_BUCKET=instant-test-backups \
    R2_ACCESS_KEY_ID=must-not-be-read R2_SECRET_ACCESS_KEY=must-not-be-exported \
    AWS_ACCESS_KEY_ID=must-not-reach-wrangler \
    WRANGLER_BIN="$fake_wrangler" INSTANT_SELF_HOST_ENV_FILE="$env_file" \
    "$script_dir/r2-wrangler.sh" "$@"
}

run_r2 provision
run_r2 provision
run_r2 verify
run_r2 configure-cors https://dashboard.example.test
run_r2 configure-lifecycle
run_r2 canary

test -f "$state/bucket-instant-test-files"
test -f "$state/bucket-instant-test-backups"
grep -Fq '"origins": ["https://dashboard.example.test"]' "$state/cors-instant-test-files.json"
grep -Fq '"origins": ["https://dashboard.example.test"]' "$state/cors-instant-test-backups.json"
grep -Fq '"exposeHeaders": ["ETag", "Content-Encoding"]' "$state/cors-instant-test-backups.json"
grep -Fq '"methods": ["GET", "HEAD", "PUT", "DELETE"]' "$state/cors-instant-test-files.json"
grep -Fq '"methods": ["GET", "HEAD"]' "$state/cors-instant-test-backups.json"
grep -Fq '"expiration": {"days": 30}' "$state/lifecycle-instant-test-backups.json"
test -z "$(find "$state/objects" -type f -print -quit)"
if grep -Eq 'must-not-be-read|must-not-be-exported' "$tmp_dir/wrangler.log"; then
  echo "R2 management tooling leaked or forwarded static credentials" >&2
  exit 1
fi

if run_r2 configure-cors https://dashboard.example.test/path >/dev/null 2>&1; then
  echo "CORS origin with a path was accepted" >&2
  exit 1
fi

echo "R2 Wrangler tooling verified"
