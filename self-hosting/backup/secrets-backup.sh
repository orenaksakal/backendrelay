#!/usr/bin/env bash
# Export the server_secrets volume, age-encrypt it and record a checksum.
#
# The volume holds `/home/instant/config/secrets.env`, the AEAD, webhook and
# rate-limit keys. Back it up separately from the database: the recovery.py
# bundle contains the same file, but it lives in the same bucket and with the
# same credentials as the database copy. This script produces the independent
# age-encrypted artifact for R2 plus one offline copy (P7.4).
#
# The age private identity stays offline. Only the public recipient is needed
# here; pass an identity file if you want the round trip verified immediately.
set -Eeuo pipefail
umask 077

usage() {
  cat <<'EOF'
usage: secrets-backup.sh [--help]

Environment:
  SECRETS_AGE_RECIPIENT      Required age public recipient (age1...). Not a secret.
  SECRETS_BACKUP_DIR         Output directory (default /var/lib/backendrelay/secrets-backups).
  SECRETS_AGE_IDENTITY_FILE  Optional offline age identity. When set, the new
                             bundle is decrypted and compared with the payload.
  SECRETS_UPLOAD_COMMAND     Optional command run with `bash -c` to copy the
                             bundle off host. BUNDLE, CHECKSUM, BUNDLE_NAME and
                             CHECKSUM_NAME are exported.
  SECRETS_VERIFY_COMMAND     Optional command proving the off-host copy exists;
                             it receives the same variables.
  SECRETS_VOLUME             Docker volume to export (default
                             <DEPLOY_COMPOSE_PROJECT>_server_secrets).
  DEPLOY_COMPOSE_PROJECT     Compose project name (default backendrelay).
  SECRETS_SOURCE_DIR         Read this host directory instead of the volume.
                             For drills only; it is never a second live copy.

The script reads the Docker volume mountpoint, so run it as root (or as the
rootless Docker user). It never prints secret values.
EOF
}

die() {
  printf 'secrets-backup: %s\n' "$*" >&2
  exit 1
}

log() {
  printf 'secrets-backup: %s\n' "$*" >&2
}

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
  '') ;;
  *)
    printf 'secrets-backup: unknown argument: %s\n' "$1" >&2
    usage >&2
    exit 2
    ;;
esac

recipient=${SECRETS_AGE_RECIPIENT:-}
out_dir=${SECRETS_BACKUP_DIR:-/var/lib/backendrelay/secrets-backups}
project=${DEPLOY_COMPOSE_PROJECT:-backendrelay}
volume=${SECRETS_VOLUME:-${project}_server_secrets}
identity=${SECRETS_AGE_IDENTITY_FILE:-}

[ -n "$recipient" ] ||
  die "set SECRETS_AGE_RECIPIENT to the operator's age public recipient (age1...)"
case "$recipient" in
  age1*) ;;
  *) die "SECRETS_AGE_RECIPIENT must be an age1... public recipient" ;;
esac
command -v age >/dev/null 2>&1 ||
  die "age is not installed; install it first (see provision/README.md)"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
if [ -n "$identity" ] && [ ! -r "$identity" ]; then
  die "cannot read SECRETS_AGE_IDENTITY_FILE: $identity"
fi

mkdir -p -- "$out_dir"
chmod 700 -- "$out_dir"

temporary=$(mktemp -d "$out_dir/.staging.XXXXXX")
cleanup() {
  [ -z "${pending:-}" ] || rm -f -- "$pending"
  [ -z "${temporary:-}" ] || rm -rf -- "$temporary"
}
trap cleanup EXIT

mkdir -p "$temporary/config"
if [ -n "${SECRETS_SOURCE_DIR:-}" ]; then
  [ -d "$SECRETS_SOURCE_DIR" ] || die "SECRETS_SOURCE_DIR is not a directory: $SECRETS_SOURCE_DIR"
  cp -a -- "$SECRETS_SOURCE_DIR/." "$temporary/config/"
else
  command -v docker >/dev/null 2>&1 || die "docker is not installed"
  if ! mountpoint=$(docker volume inspect --format '{{.Mountpoint}}' "$volume" 2>/dev/null); then
    die "docker volume $volume does not exist; check DEPLOY_COMPOSE_PROJECT or set SECRETS_VOLUME"
  fi
  [ -d "$mountpoint" ] || die "docker volume $volume has no readable mountpoint"
  cp -a -- "$mountpoint/." "$temporary/config/"
fi

secrets_file=$temporary/config/secrets.env
[ -s "$secrets_file" ] || die "no non-empty secrets.env in the server config volume"
for name in INSTANT_AEAD_KEYSET_JSON INSTANT_WEBHOOK_KEYSET_JSON INSTANT_RATE_LIMIT_HMAC_KEY; do
  grep -q "^${name}=" "$secrets_file" ||
    die "secrets.env is incomplete: ${name} is missing"
done
chmod 600 -- "$secrets_file"

payload=$temporary/payload.tar
tar --create --file "$payload" --directory "$temporary" \
  --exclude='*.tmp' --numeric-owner --format=posix config

stamp=$(date -u +%Y%m%dT%H%M%SZ)
host=$(hostname -s 2>/dev/null || printf 'host')
host=${host//[^A-Za-z0-9._-]/_}
bundle_name="secrets-${stamp}-${host}.tar.age"
bundle=$out_dir/$bundle_name
checksum=$out_dir/$bundle_name.sha256
pending=$out_dir/.${bundle_name}.part
[ ! -e "$bundle" ] || die "refusing to overwrite the existing bundle $bundle"

age --encrypt --recipient "$recipient" --output "$pending" "$payload"
mv -f -- "$pending" "$bundle"

if [ -n "$identity" ]; then
  age --decrypt --identity "$identity" --output "$temporary/decrypted.tar" "$bundle"
  cmp -s -- "$payload" "$temporary/decrypted.tar" ||
    die "the encrypted bundle does not decrypt to the exported payload"
  log "round-trip verified with the offline identity"
else
  log "WARNING: no SECRETS_AGE_IDENTITY_FILE; verify the copy with the offline identity"
fi

(
  cd -- "$out_dir"
  sha256sum -- "$bundle_name" >"$bundle_name.sha256"
)
chmod 600 -- "$bundle" "$checksum"

if [ -n "${SECRETS_UPLOAD_COMMAND:-}" ]; then
  BUNDLE=$bundle CHECKSUM=$checksum BUNDLE_NAME=$bundle_name \
    CHECKSUM_NAME=$bundle_name.sha256 bash -c "$SECRETS_UPLOAD_COMMAND"
  if [ -n "${SECRETS_VERIFY_COMMAND:-}" ]; then
    BUNDLE=$bundle CHECKSUM=$checksum BUNDLE_NAME=$bundle_name \
      CHECKSUM_NAME=$bundle_name.sha256 bash -c "$SECRETS_VERIFY_COMMAND"
  else
    log "WARNING: SECRETS_UPLOAD_COMMAND ran without SECRETS_VERIFY_COMMAND; off-host presence is unverified"
  fi
else
  log "no SECRETS_UPLOAD_COMMAND; the bundle is local-only until you copy it"
fi

printf 'bundle=%s\n' "$bundle"
printf 'sha256=%s\n' "$(cut -d ' ' -f 1 -- "$checksum")"
log "copy the bundle and its .sha256 to one offline medium as well; keep the age identity offline"
