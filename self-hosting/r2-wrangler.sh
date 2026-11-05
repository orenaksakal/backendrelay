#!/bin/sh
set -eu

# Manage InstantDB's Cloudflare R2 buckets with Wrangler's OAuth session.
# Static R2 S3 credentials are deliberately neither read nor exported here.

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
files_bucket=${R2_FILES_BUCKET:-instantdb-files}
backups_bucket=${R2_BACKUPS_BUCKET:-instantdb-backups}

validate_bucket() {
  bucket=$1
  label=$2
  length=${#bucket}
  if [ "$length" -lt 3 ] || [ "$length" -gt 63 ]; then
    echo "$label must contain between 3 and 63 characters" >&2
    exit 2
  fi
  case "$bucket" in
    [a-z0-9]*[a-z0-9]) ;;
    *) echo "$label must start and end with a lowercase letter or digit" >&2; exit 2 ;;
  esac
  case "$bucket" in
    *[!a-z0-9-]*) echo "$label may contain only lowercase letters, digits, and hyphens" >&2; exit 2 ;;
  esac
}

validate_bucket "$files_bucket" R2_FILES_BUCKET
validate_bucket "$backups_bucket" R2_BACKUPS_BUCKET
if [ "$files_bucket" = "$backups_bucket" ]; then
  echo "R2_FILES_BUCKET and R2_BACKUPS_BUCKET must be different buckets" >&2
  exit 2
fi

run_wrangler() {
  (
    # Wrangler authenticates independently. Do not give this subprocess either
    # credential family or the path of the file that contains them.
    unset INSTANT_SELF_HOST_ENV_FILE R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
    unset INSTANT_S3_ACCESS_KEY_ID INSTANT_S3_SECRET_ACCESS_KEY INSTANT_S3_SESSION_TOKEN
    unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN
    cd "${TMPDIR:-/tmp}"
    if [ -n "${WRANGLER_BIN:-}" ]; then
      "$WRANGLER_BIN" "$@"
    elif command -v wrangler >/dev/null 2>&1; then
      wrangler "$@"
    elif command -v npx >/dev/null 2>&1; then
      npx --yes wrangler@latest "$@"
    else
      echo "Wrangler 4.x is required (install wrangler or Node.js/npx)" >&2
      exit 1
    fi
  )
}

require_auth() {
  if ! run_wrangler whoami >/dev/null; then
    echo "Wrangler is not authenticated; run 'npx wrangler login' and retry" >&2
    exit 1
  fi
}

provision_bucket() {
  bucket=$1
  if run_wrangler r2 bucket info "$bucket" --json >/dev/null 2>&1; then
    echo "R2 bucket already exists: $bucket"
  else
    echo "Creating R2 bucket: $bucket"
    run_wrangler r2 bucket create "$bucket"
  fi
  run_wrangler r2 bucket info "$bucket" --json >/dev/null
}

verify_bucket() {
  bucket=$1
  run_wrangler r2 bucket info "$bucket" --json >/dev/null
  echo "Verified R2 bucket: $bucket"
}

canary_bucket() {
  bucket=$1
  tmp_dir=$2
  key="instantdb-canary/$(date -u +%Y%m%dT%H%M%SZ)-$$-$bucket"
  object="$bucket/$key"
  source_file="$tmp_dir/source"
  downloaded_file="$tmp_dir/downloaded"
  printf 'instantdb-r2-canary:%s\n' "$key" >"$source_file"
  rm -f "$downloaded_file"

  uploaded=0
  cleanup_object() {
    if [ "$uploaded" -eq 1 ]; then
      run_wrangler r2 object delete "$object" --remote >/dev/null 2>&1 || true
    fi
  }
  trap 'cleanup_object; rm -rf "$tmp_dir"' EXIT HUP INT TERM

  run_wrangler r2 object put "$object" --remote --file "$source_file" \
    --content-type text/plain --force >/dev/null
  uploaded=1
  run_wrangler r2 object get "$object" --remote --file "$downloaded_file" >/dev/null
  if ! cmp -s "$source_file" "$downloaded_file"; then
    echo "R2 canary payload mismatch for $bucket" >&2
    exit 1
  fi
  run_wrangler r2 object delete "$object" --remote >/dev/null
  uploaded=0
  trap - EXIT HUP INT TERM
  rm -rf "$tmp_dir"
  echo "R2 put/get/delete canary passed: $bucket"
}

configure_cors() {
  origin=$1
  if ! printf '%s\n' "$origin" | grep -Eq '^https://[A-Za-z0-9.-]+(:[0-9]+)?$|^http://(localhost|127\.0\.0\.1)(:[0-9]+)?$'; then
    echo "CORS origin must be an https origin (or localhost development origin), without a path" >&2
    exit 2
  fi
  files_tmp=$(mktemp)
  backups_tmp=$(mktemp)
  trap 'rm -f "$files_tmp" "$backups_tmp"' EXIT HUP INT TERM
  sed "s|ORIGIN_PLACEHOLDER|$origin|g" "$script_dir/r2-cors.json" >"$files_tmp"
  sed "s|ORIGIN_PLACEHOLDER|$origin|g" "$script_dir/r2-backup-cors.json" >"$backups_tmp"
  run_wrangler r2 bucket cors set "$files_bucket" --file "$files_tmp" --force >/dev/null
  run_wrangler r2 bucket cors set "$backups_bucket" --file "$backups_tmp" --force >/dev/null
  run_wrangler r2 bucket cors list "$files_bucket" >/dev/null
  run_wrangler r2 bucket cors list "$backups_bucket" >/dev/null
  trap - EXIT HUP INT TERM
  rm -f "$files_tmp" "$backups_tmp"
  echo "Verified R2 browser CORS policy: $files_bucket, $backups_bucket ($origin)"
}

configure_lifecycle() {
  run_wrangler r2 bucket lifecycle set "$backups_bucket" --file "$script_dir/r2-backup-lifecycle.json" --force >/dev/null
  run_wrangler r2 bucket lifecycle list "$backups_bucket" >/dev/null
  echo "Verified R2 backup lifecycle policy: $backups_bucket (expire after 30 days)"
}

usage() {
  cat >&2 <<EOF
usage: $0 <command> [arguments]

Commands:
  auth                         verify Wrangler OAuth authentication
  provision                    idempotently create and verify both buckets
  verify                       verify that both buckets are accessible
  canary                       put, get, compare, and delete in both buckets
  configure-cors <origin>      set and verify browser CORS on both buckets
  configure-lifecycle          expire per-app backup objects after 30 days
  ready <origin>               provision, configure CORS and lifecycle, verify, and canary

Bucket names come only from R2_FILES_BUCKET and R2_BACKUPS_BUCKET, with
instantdb-files and instantdb-backups as defaults. This tool never opens .env.
No R2 access-key ID or secret is read by this tool.
EOF
  exit 2
}

command=${1:-}
case "$command" in
  auth)
    [ "$#" -eq 1 ] || usage
    require_auth
    echo "Wrangler authentication verified"
    ;;
  provision)
    [ "$#" -eq 1 ] || usage
    require_auth
    provision_bucket "$files_bucket"
    provision_bucket "$backups_bucket"
    ;;
  verify)
    [ "$#" -eq 1 ] || usage
    require_auth
    verify_bucket "$files_bucket"
    verify_bucket "$backups_bucket"
    ;;
  canary)
    [ "$#" -eq 1 ] || usage
    require_auth
    canary_bucket "$files_bucket" "$(mktemp -d)"
    canary_bucket "$backups_bucket" "$(mktemp -d)"
    ;;
  configure-cors)
    [ "$#" -eq 2 ] || usage
    require_auth
    configure_cors "$2"
    ;;
  configure-lifecycle)
    [ "$#" -eq 1 ] || usage
    require_auth
    configure_lifecycle
    ;;
  ready)
    [ "$#" -eq 2 ] || usage
    require_auth
    provision_bucket "$files_bucket"
    provision_bucket "$backups_bucket"
    configure_cors "$2"
    configure_lifecycle
    verify_bucket "$files_bucket"
    verify_bucket "$backups_bucket"
    canary_bucket "$files_bucket" "$(mktemp -d)"
    canary_bucket "$backups_bucket" "$(mktemp -d)"
    ;;
  *) usage ;;
esac
