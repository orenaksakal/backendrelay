#!/bin/sh
set -eu

# Optional second-stage check of the exact Rust S3-compatible adapter. Unlike
# the Wrangler canary, this needs the bucket-scoped credentials used by the
# running server. Values are sourced locally and are never printed.

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
env_file=${INSTANT_SELF_HOST_ENV_FILE:-"$script_dir/.env"}

read_env_value() {
  name=$1
  awk -v name="$name" '
    BEGIN {
      prefix = "^[[:space:]]*(export[[:space:]]+)?" name "[[:space:]]*="
    }
    $0 ~ prefix {
      line = $0
      sub(/\r$/, "", line)
      sub(prefix, "", line)
      sub(/^[[:space:]]+/, "", line)
      sub(/[[:space:]]+$/, "", line)
      value = line
      count += 1
    }
    END {
      if (count == 0) exit 1
      if (count != 1) exit 2
      print value
    }
  ' "$env_file"
}

# Remove one matching pair of quotes without evaluating escapes, substitutions,
# or any other shell syntax from the operator file.
unquote_env_value() {
  value=$1
  case "$value" in
    \"*\") value=${value#\"}; value=${value%\"} ;;
    \'*\') value=${value#\'}; value=${value%\'} ;;
  esac
  printf '%s\n' "$value"
}

load_missing_value() {
  name=$1
  current=$2
  [ -n "$current" ] && return 0

  if value=$(read_env_value "$name"); then
    value=$(unquote_env_value "$value")
  else
    status=$?
    if [ "$status" -eq 2 ]; then
      echo "$name must occur exactly once in $env_file" >&2
      exit 1
    fi
    value=
  fi

  case "$name" in
    R2_ACCOUNT_ID) R2_ACCOUNT_ID=$value ;;
    R2_ACCESS_KEY_ID) R2_ACCESS_KEY_ID=$value ;;
    R2_SECRET_ACCESS_KEY) R2_SECRET_ACCESS_KEY=$value ;;
    R2_FILES_BUCKET) R2_FILES_BUCKET=$value ;;
    R2_BACKUPS_BUCKET) R2_BACKUPS_BUCKET=$value ;;
  esac
}

# Non-empty process environment values take precedence. Only open the local
# file if at least one value is missing, and never source or execute its text.
if [ -z "${R2_ACCOUNT_ID:-}" ] || [ -z "${R2_ACCESS_KEY_ID:-}" ] || \
   [ -z "${R2_SECRET_ACCESS_KEY:-}" ] || [ -z "${R2_FILES_BUCKET:-}" ] || \
   [ -z "${R2_BACKUPS_BUCKET:-}" ]; then
  if [ ! -f "$env_file" ]; then
    echo "R2 values must be set in the process environment or a mode-600 $env_file" >&2
    exit 1
  fi
  env_mode=$(stat -c '%a' "$env_file" 2>/dev/null || stat -f '%Lp' "$env_file" 2>/dev/null || true)
  if [ "$env_mode" != 600 ]; then
    echo "$env_file contains R2 credentials and must have mode 600" >&2
    exit 1
  fi

  load_missing_value R2_ACCOUNT_ID "${R2_ACCOUNT_ID:-}"
  load_missing_value R2_ACCESS_KEY_ID "${R2_ACCESS_KEY_ID:-}"
  load_missing_value R2_SECRET_ACCESS_KEY "${R2_SECRET_ACCESS_KEY:-}"
  load_missing_value R2_FILES_BUCKET "${R2_FILES_BUCKET:-}"
  load_missing_value R2_BACKUPS_BUCKET "${R2_BACKUPS_BUCKET:-}"
fi

require_value() {
  name=$1
  value=$2
  if [ -z "$value" ]; then
    echo "$name must be set in the process environment or $env_file" >&2
    exit 1
  fi
}
require_value R2_ACCOUNT_ID "${R2_ACCOUNT_ID:-}"
require_value R2_ACCESS_KEY_ID "${R2_ACCESS_KEY_ID:-}"
require_value R2_SECRET_ACCESS_KEY "${R2_SECRET_ACCESS_KEY:-}"
require_value R2_FILES_BUCKET "${R2_FILES_BUCKET:-}"
require_value R2_BACKUPS_BUCKET "${R2_BACKUPS_BUCKET:-}"
if ! printf '%s\n' "$R2_ACCOUNT_ID" | grep -Eq '^[0-9a-f]{32}$'; then
  echo "R2_ACCOUNT_ID must be a 32-character lowercase hexadecimal ID" >&2
  exit 1
fi
validate_bucket() {
  bucket=$1
  label=$2
  if ! printf '%s\n' "$bucket" | grep -Eq '^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$'; then
    echo "$label must be a 3-63 character lowercase R2 bucket name" >&2
    exit 1
  fi
}
validate_bucket "$R2_FILES_BUCKET" R2_FILES_BUCKET
validate_bucket "$R2_BACKUPS_BUCKET" R2_BACKUPS_BUCKET
if [ "$R2_FILES_BUCKET" = "$R2_BACKUPS_BUCKET" ]; then
  echo "R2_FILES_BUCKET and R2_BACKUPS_BUCKET must be different buckets" >&2
  exit 1
fi

export INSTANT_LIVE_S3_ENDPOINT="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"
export INSTANT_LIVE_S3_REGION=auto
export INSTANT_LIVE_S3_ACCESS_KEY=$R2_ACCESS_KEY_ID
export INSTANT_LIVE_S3_SECRET_KEY=$R2_SECRET_ACCESS_KEY
export INSTANT_LIVE_S3_FILES_BUCKET=$R2_FILES_BUCKET
export INSTANT_LIVE_S3_BACKUPS_BUCKET=$R2_BACKUPS_BUCKET
export S3_BUCKET=$R2_FILES_BUCKET
export S3_APP_BACKUPS_BUCKET=$R2_BACKUPS_BUCKET
files_bucket=$R2_FILES_BUCKET
backups_bucket=$R2_BACKUPS_BUCKET

# Do not forward the input names as an unnecessary second copy. The adapter
# receives only its purpose-specific live-test variables above.
unset R2_ACCOUNT_ID R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY R2_FILES_BUCKET R2_BACKUPS_BUCKET

cd "$script_dir/../server-rs"
run_bucket_canary() {
  bucket=$1
  secondary_bucket=$2
  export INSTANT_LIVE_S3_BUCKET=$bucket
  export INSTANT_LIVE_S3_SECONDARY_BUCKET=$secondary_bucket
  echo "Running native Rust R2 adapter canary: $bucket"
  if [ -n "${INSTANT_LIVE_S3_TEST_BINARY:-}" ]; then
    if [ ! -x "$INSTANT_LIVE_S3_TEST_BINARY" ]; then
      echo "INSTANT_LIVE_S3_TEST_BINARY must name an executable test binary" >&2
      exit 1
    fi
    "$INSTANT_LIVE_S3_TEST_BINARY" \
      s3_storage::tests::live_s3_put_get_delete_round_trip_when_configured \
      --exact --nocapture
  else
    "${CARGO_BIN:-cargo}" test -p instant-server --no-default-features \
      s3_storage::tests::live_s3_put_get_delete_round_trip_when_configured \
      -- --exact --nocapture
  fi
  echo "Native Rust R2 adapter canary passed: $bucket"
}

run_bucket_canary "$files_bucket" "$backups_bucket"
run_bucket_canary "$backups_bucket" "$files_bucket"
