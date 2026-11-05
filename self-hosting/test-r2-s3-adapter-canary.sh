#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

fake_cargo="$tmp_dir/cargo"
cat >"$fake_cargo" <<'EOF'
#!/bin/sh
set -eu
test "$INSTANT_LIVE_S3_ENDPOINT" = "$EXPECTED_ENDPOINT"
test "$INSTANT_LIVE_S3_REGION" = auto
test "$INSTANT_LIVE_S3_ACCESS_KEY" = "$EXPECTED_ACCESS_KEY"
test "$INSTANT_LIVE_S3_SECRET_KEY" = "$EXPECTED_SECRET_KEY"
test "$INSTANT_LIVE_S3_FILES_BUCKET" = "$EXPECTED_FILES_BUCKET"
test "$INSTANT_LIVE_S3_BACKUPS_BUCKET" = "$EXPECTED_BACKUPS_BUCKET"
test "$S3_BUCKET" = "$EXPECTED_FILES_BUCKET"
test "$S3_APP_BACKUPS_BUCKET" = "$EXPECTED_BACKUPS_BUCKET"
test "${R2_ACCOUNT_ID+x}${R2_ACCESS_KEY_ID+x}${R2_SECRET_ACCESS_KEY+x}" = ""
test "${R2_FILES_BUCKET+x}${R2_BACKUPS_BUCKET+x}" = ""
printf '%s\n' "$INSTANT_LIVE_S3_BUCKET" >>"$CANARY_CAPTURE"
printf '%s\n' "$INSTANT_LIVE_S3_SECONDARY_BUCKET" >>"$CANARY_SECONDARY_CAPTURE"
printf '%s\n' "$*" >>"$CANARY_ARGS"
EOF
chmod +x "$fake_cargo"

assert_two_buckets() {
  capture=$1
  files=$2
  backups=$3
  secondary_capture=$4
  test "$(sed -n '1p' "$capture")" = "$files"
  test "$(sed -n '2p' "$capture")" = "$backups"
  test "$(wc -l <"$capture" | tr -d ' ')" -eq 2
  test "$(sed -n '1p' "$secondary_capture")" = "$backups"
  test "$(sed -n '2p' "$secondary_capture")" = "$files"
  test "$(wc -l <"$secondary_capture" | tr -d ' ')" -eq 2
}

run_canary() {
  output=$1
  shift
  env R2_ACCOUNT_ID= R2_ACCESS_KEY_ID= R2_SECRET_ACCESS_KEY= \
    R2_FILES_BUCKET= R2_BACKUPS_BUCKET= \
    CARGO_BIN="$fake_cargo" CANARY_CAPTURE="$tmp_dir/capture" \
    CANARY_SECONDARY_CAPTURE="$tmp_dir/secondary-capture" \
    CANARY_ARGS="$tmp_dir/args" "$@" \
    "$script_dir/run-r2-s3-adapter-canary.sh" >"$output" 2>&1
}

file_env="$tmp_dir/file.env"
cat >"$file_env" <<EOF
export R2_ACCOUNT_ID = "0123456789abcdef0123456789abcdef"
R2_ACCESS_KEY_ID='file-access-sensitive'
R2_SECRET_ACCESS_KEY='file-secret-sensitive\$(touch $tmp_dir/should-never-exist)'
R2_FILES_BUCKET='fixture-files'
R2_BACKUPS_BUCKET="fixture-backups"
IGNORED=\$(touch $tmp_dir/ignored-should-never-exist)
EOF
malicious_secret=$(printf 'file-secret-sensitive$(touch %s/should-never-exist)' "$tmp_dir")
chmod 600 "$file_env"
: >"$tmp_dir/capture"
: >"$tmp_dir/secondary-capture"
: >"$tmp_dir/args"
run_canary "$tmp_dir/file.out" \
  INSTANT_SELF_HOST_ENV_FILE="$file_env" \
  EXPECTED_ENDPOINT=https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com \
  EXPECTED_ACCESS_KEY=file-access-sensitive \
  EXPECTED_SECRET_KEY="$malicious_secret" \
  EXPECTED_FILES_BUCKET=fixture-files EXPECTED_BACKUPS_BUCKET=fixture-backups
assert_two_buckets "$tmp_dir/capture" fixture-files fixture-backups "$tmp_dir/secondary-capture"
test ! -e "$tmp_dir/should-never-exist"
test ! -e "$tmp_dir/ignored-should-never-exist"
grep -Fq 'test -p instant-server --no-default-features' "$tmp_dir/args"

# A prebuilt test binary keeps production credentials away from Cargo build
# scripts and procedural macros in the manual GitHub canary.
fake_test_binary="$tmp_dir/instant-server-test"
cat >"$fake_test_binary" <<'EOF'
#!/bin/sh
set -eu
test "$1" = s3_storage::tests::live_s3_put_get_delete_round_trip_when_configured
test "$2" = --exact
test "$3" = --nocapture
printf '%s\n' "$INSTANT_LIVE_S3_BUCKET" >>"$PREBUILT_CAPTURE"
printf '%s\n' "$INSTANT_LIVE_S3_SECONDARY_BUCKET" >>"$PREBUILT_SECONDARY_CAPTURE"
EOF
chmod +x "$fake_test_binary"
: >"$tmp_dir/prebuilt-capture"
: >"$tmp_dir/prebuilt-secondary-capture"
env R2_ACCOUNT_ID=fedcba9876543210fedcba9876543210 \
  R2_ACCESS_KEY_ID=prebuilt-access R2_SECRET_ACCESS_KEY=prebuilt-secret \
  R2_FILES_BUCKET=prebuilt-files R2_BACKUPS_BUCKET=prebuilt-backups \
  INSTANT_LIVE_S3_TEST_BINARY="$fake_test_binary" PREBUILT_CAPTURE="$tmp_dir/prebuilt-capture" \
  PREBUILT_SECONDARY_CAPTURE="$tmp_dir/prebuilt-secondary-capture" \
  CARGO_BIN="$tmp_dir/does-not-exist" \
  "$script_dir/run-r2-s3-adapter-canary.sh" >"$tmp_dir/prebuilt.out" 2>&1
assert_two_buckets "$tmp_dir/prebuilt-capture" prebuilt-files prebuilt-backups \
  "$tmp_dir/prebuilt-secondary-capture"

# Complete process injection takes precedence and does not open an insecure or
# absent env file.
chmod 644 "$file_env"
: >"$tmp_dir/capture"
: >"$tmp_dir/secondary-capture"
run_canary "$tmp_dir/process.out" \
  INSTANT_SELF_HOST_ENV_FILE="$file_env" \
  R2_ACCOUNT_ID=fedcba9876543210fedcba9876543210 \
  R2_ACCESS_KEY_ID=process-access-sensitive R2_SECRET_ACCESS_KEY=process-secret-sensitive \
  R2_FILES_BUCKET=process-files R2_BACKUPS_BUCKET=process-backups \
  EXPECTED_ENDPOINT=https://fedcba9876543210fedcba9876543210.r2.cloudflarestorage.com \
  EXPECTED_ACCESS_KEY=process-access-sensitive EXPECTED_SECRET_KEY=process-secret-sensitive \
  EXPECTED_FILES_BUCKET=process-files EXPECTED_BACKUPS_BUCKET=process-backups
assert_two_buckets "$tmp_dir/capture" process-files process-backups "$tmp_dir/secondary-capture"

# A partial process environment uses the protected file only for missing keys.
chmod 600 "$file_env"
: >"$tmp_dir/capture"
: >"$tmp_dir/secondary-capture"
run_canary "$tmp_dir/precedence.out" \
  INSTANT_SELF_HOST_ENV_FILE="$file_env" R2_FILES_BUCKET=injected-files \
  EXPECTED_ENDPOINT=https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com \
  EXPECTED_ACCESS_KEY=file-access-sensitive \
  EXPECTED_SECRET_KEY="$malicious_secret" \
  EXPECTED_FILES_BUCKET=injected-files EXPECTED_BACKUPS_BUCKET=fixture-backups
assert_two_buckets "$tmp_dir/capture" injected-files fixture-backups "$tmp_dir/secondary-capture"

expect_failure() {
  label=$1
  shift
  if env R2_ACCOUNT_ID= R2_ACCESS_KEY_ID= R2_SECRET_ACCESS_KEY= \
      R2_FILES_BUCKET= R2_BACKUPS_BUCKET= CARGO_BIN="$fake_cargo" "$@" \
      "$script_dir/run-r2-s3-adapter-canary.sh" \
      >"$tmp_dir/$label.out" 2>&1; then
    echo "$label unexpectedly succeeded" >&2
    exit 1
  fi
}

chmod 644 "$file_env"
expect_failure insecure INSTANT_SELF_HOST_ENV_FILE="$file_env"
chmod 600 "$file_env"
expect_failure invalid-account INSTANT_SELF_HOST_ENV_FILE="$file_env" R2_ACCOUNT_ID=not-an-account
expect_failure duplicate-buckets INSTANT_SELF_HOST_ENV_FILE="$tmp_dir/missing.env" \
  R2_ACCOUNT_ID=0123456789abcdef0123456789abcdef R2_ACCESS_KEY_ID=access \
  R2_SECRET_ACCESS_KEY=secret R2_FILES_BUCKET=same-bucket R2_BACKUPS_BUCKET=same-bucket
expect_failure invalid-bucket INSTANT_SELF_HOST_ENV_FILE="$tmp_dir/missing.env" \
  R2_ACCOUNT_ID=0123456789abcdef0123456789abcdef R2_ACCESS_KEY_ID=access \
  R2_SECRET_ACCESS_KEY=secret R2_FILES_BUCKET=Bad_Bucket R2_BACKUPS_BUCKET=valid-backups
expect_failure missing INSTANT_SELF_HOST_ENV_FILE="$tmp_dir/missing.env"

duplicate_env="$tmp_dir/duplicate.env"
cat >"$duplicate_env" <<'EOF'
R2_ACCOUNT_ID=0123456789abcdef0123456789abcdef
R2_ACCOUNT_ID=fedcba9876543210fedcba9876543210
R2_ACCESS_KEY_ID=duplicate-access-sensitive
R2_SECRET_ACCESS_KEY=duplicate-secret-sensitive
R2_FILES_BUCKET=duplicate-files
R2_BACKUPS_BUCKET=duplicate-backups
EOF
chmod 600 "$duplicate_env"
expect_failure duplicate-key INSTANT_SELF_HOST_ENV_FILE="$duplicate_env"

if grep -R -E 'file-(access|secret)-sensitive|process-(access|secret)-sensitive|duplicate-(access|secret)-sensitive' \
    "$tmp_dir"/*.out >/dev/null; then
  echo "R2 canary output exposed a credential" >&2
  exit 1
fi

echo "R2 S3 adapter canary wrapper verified"
