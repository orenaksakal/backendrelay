#!/usr/bin/env bash
# Provision a clean Ubuntu 24.04 host for the BackendRelay production stack.
#
# Usage:
#   sudo ADMIN_SSH_CIDR=203.0.113.7/32 bash self-hosting/provision/host-bootstrap.sh
#   sudo bash self-hosting/provision/host-bootstrap.sh --check
#   sudo ADMIN_SSH_CIDR=... bash ... --cloudflare-ips ./cloudflare-cidrs.txt
#
# The script installs Docker, enables host firewall rules for SSH and
# Cloudflare, makes SSH key-only, enables unattended security updates and sets
# the Docker log limits from P2.11. It is idempotent, except that a changed
# Docker daemon configuration is only applied while no containers run.
#
# --check verifies the result without changing anything and exits non-zero on a
# finding. It needs the Cloudflare ranges to verify the 80/443 rules; pass
# --cloudflare-ips when the host cannot reach cloudflare.com.
#
# The Hetzner Cloud Firewall is configured in the console, not here, and is the
# authoritative filter: Docker publishes ports through its own iptables chain,
# which bypasses ufw INPUT rules. See README.md.
#
# This script does not touch Cloudflare DNS, the WAF, R2, PostgreSQL, email or
# registry credentials. Those are owner steps in README.md.
set -Eeuo pipefail
umask 022

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
daemon_config=$script_dir/docker-daemon.json
mode=apply
cloudflare_ips_file=${CLOUDFLARE_IPS_FILE:-}
admin_ssh_cidr=${ADMIN_SSH_CIDR:-}
ssh_port=${SSH_PORT:-22}
deploy_root=${DEPLOY_ROOT:-/opt/backendrelay}

usage() {
  cat <<'EOF'
usage: host-bootstrap.sh [--check] [--cloudflare-ips FILE]

Environment:
  ADMIN_SSH_CIDR      Comma-separated admin source CIDRs allowed on SSH_PORT
                      (required unless --check is used)
  SSH_PORT            SSH port to open (default 22)
  DEPLOY_ROOT         Repository checkout on the host (default /opt/backendrelay)
  CLOUDFLARE_IPS_FILE Same as --cloudflare-ips

Flags:
  --check             Verify the host state; change nothing
  --cloudflare-ips F  Read Cloudflare IPv4/IPv6 ranges from F instead of
                      downloading https://www.cloudflare.com/ips-v4 and ips-v6
  -h, --help          Show this message
EOF
}

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

warn() {
  log "WARNING: $*" >&2
}

die() {
  log "ERROR: $*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --check)
      mode=check
      ;;
    --cloudflare-ips)
      [ "$#" -ge 2 ] || { echo "ERROR: --cloudflare-ips needs a file" >&2; usage >&2; exit 2; }
      cloudflare_ips_file=$2
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

[ "$(id -u)" -eq 0 ] || die "run this script as root (or with sudo)"

# shellcheck source=/dev/null
. /etc/os-release
[ "${ID:-}" = ubuntu ] || die "expected Ubuntu, found ${ID:-an unknown distribution}"
[ "${VERSION_ID:-}" = 24.04 ] || die "expected Ubuntu 24.04, found ${VERSION_ID:-unknown}"

if [ -n "$admin_ssh_cidr" ]; then
  admin_ssh_cidrs=${admin_ssh_cidr//,/ }
  for cidr in $admin_ssh_cidrs; do
    [[ "$cidr" =~ ^[0-9A-Fa-f:.]+/[0-9]{1,3}$ ]] || die "ADMIN_SSH_CIDR is not a CIDR: $cidr"
  done
fi
[ "$mode" = check ] || [ -n "${admin_ssh_cidrs:-}" ] || \
  die "set ADMIN_SSH_CIDR to the network you log in from (for example 203.0.113.7/32)"

read_cloudflare_cidrs() {
  if [ -n "$cloudflare_ips_file" ]; then
    [ -r "$cloudflare_ips_file" ] || die "cannot read Cloudflare range file: $cloudflare_ips_file"
    sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$cloudflare_ips_file"
    return
  fi
  curl -fsS --max-time 30 https://www.cloudflare.com/ips-v4 || \
    die "could not download Cloudflare ranges; pass --cloudflare-ips FILE"
  printf '\n'
  curl -fsS --max-time 30 https://www.cloudflare.com/ips-v6 || \
    die "could not download Cloudflare ranges; pass --cloudflare-ips FILE"
}

load_cloudflare_cidrs() {
  local cidr ranges
  ranges=$(read_cloudflare_cidrs) || exit 1
  cloudflare_cidrs=()
  while IFS= read -r cidr; do
    cidr=${cidr//[[:space:]]/}
    [ -n "$cidr" ] || continue
    [[ "$cidr" =~ ^[0-9A-Fa-f:.]+/[0-9]{1,3}$ ]] || die "invalid Cloudflare range: $cidr"
    cloudflare_cidrs+=("$cidr")
  done <<<"$ranges"
  [ "${#cloudflare_cidrs[@]}" -gt 0 ] || die "the Cloudflare range list is empty"
}

# Refuse to disable password logins when no key can log in afterwards.
require_authorized_key() {
  local file
  for file in /root/.ssh/authorized_keys /home/*/.ssh/authorized_keys; do
    if [ -s "$file" ]; then
      return 0
    fi
  done
  die "no non-empty authorized_keys for root or a /home user; add the admin key before making SSH key-only"
}

install_packages() {
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  # docker.io and docker-compose-v2 come from the Ubuntu archive; deploy.sh
  # needs Compose v2, curl and python3. age encrypts the secrets backup
  # (secrets-backup.sh). cosign is a separate owner install; --check warns when
  # it is missing.
  apt-get install -y --no-install-recommends \
    ca-certificates curl python3 age ufw unattended-upgrades \
    docker.io docker-compose-v2
}

configure_ssh() {
  require_authorized_key
  install -d -m 0755 /etc/ssh/sshd_config.d
  # This file sorts before cloud-init's 50-cloud-init.conf, and sshd keeps the
  # first value it reads, so these settings win.
  cat >/etc/ssh/sshd_config.d/10-backendrelay.conf <<'EOF'
# Managed by self-hosting/provision/host-bootstrap.sh (P7.3).
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
EOF
  sshd -t
  systemctl enable ssh
  systemctl reload ssh 2>/dev/null || systemctl restart ssh
  log "SSH accepts keys only"
}

configure_ufw() {
  ufw default deny incoming
  ufw default allow outgoing
  for cidr in $admin_ssh_cidrs; do
    ufw allow from "$cidr" to any port "$ssh_port" proto tcp comment 'admin ssh'
  done
  for cidr in "${cloudflare_cidrs[@]}"; do
    ufw allow from "$cidr" to any port 80 proto tcp comment 'cloudflare http'
    ufw allow from "$cidr" to any port 443 proto tcp comment 'cloudflare https'
  done
  ufw --force enable
  log "ufw allows ${ssh_port}/tcp from the admin CIDR and 80/443 from ${#cloudflare_cidrs[@]} Cloudflare ranges"
}

configure_docker() {
  install -d -m 0755 /etc/docker
  systemctl enable --now docker
  if cmp -s "$daemon_config" /etc/docker/daemon.json; then
    log "Docker daemon configuration is already current"
  else
    install -m 0644 "$daemon_config" /etc/docker/daemon.json
    if [ -n "$(docker ps -q 2>/dev/null || true)" ]; then
      warn "Docker daemon configuration changed; restart Docker in a maintenance window"
    else
      systemctl restart docker
    fi
    log "Installed Docker log limits and no-new-privileges"
  fi
}

configure_unattended_upgrades() {
  cat >/etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
APT::Periodic::AutocleanInterval "7";
EOF
  # Security pockets only. Automatic reboot stays off: a reboot interrupts
  # live sessions, so schedule it with the drain procedure in operations.md.
  cat >/etc/apt/apt.conf.d/52-backendrelay-unattended-upgrades <<'EOF'
Unattended-Upgrade::Allowed-Origins {
        "${distro_id}:${distro_codename}-security";
        "${distro_id}ESMApps:${distro_codename}-apps-security";
        "${distro_id}ESM:${distro_codename}-infra-security";
};
Unattended-Upgrade::Remove-Unused-Dependencies "true";
Unattended-Upgrade::Automatic-Reboot "false";
EOF
  systemctl enable --now unattended-upgrades
  log "Unattended security updates enabled; reboots stay manual"
}

configure_host() {
  install -d -m 0755 "$deploy_root"
  install -d -m 0700 /etc/backendrelay
  install -d -m 0700 /var/lib/backendrelay /var/lib/backendrelay/deploy \
    /var/lib/backendrelay/secrets-backups
  timedatectl set-timezone UTC
  timedatectl set-ntp true
  log "Created $deploy_root, /etc/backendrelay and /var/lib/backendrelay (UTC, NTP on)"
}

failures=0

pass() {
  log "ok: $*"
}

fail() {
  failures=$((failures + 1))
  log "FAIL: $*" >&2
}

expect_value() {
  local description=$1 actual=$2 expected=$3
  if [ "$actual" = "$expected" ]; then
    pass "$description"
  else
    fail "$description (got '$actual', expected '$expected')"
  fi
}

check_ssh() {
  local config
  if ! config=$(sshd -T 2>/dev/null); then
    fail "sshd configuration parses"
    return
  fi
  expect_value "SSH password authentication is disabled" \
    "$(printf '%s\n' "$config" | awk '$1 == "passwordauthentication" { print $2 }')" no
  expect_value "SSH keyboard-interactive authentication is disabled" \
    "$(printf '%s\n' "$config" | awk '$1 == "kbdinteractiveauthentication" { print $2 }')" no
  expect_value "SSH root login is key-only" \
    "$(printf '%s\n' "$config" | awk '$1 == "permitrootlogin" { print ($2 == "without-password" ? "prohibit-password" : $2) }')" prohibit-password
  expect_value "SSH public-key authentication is enabled" \
    "$(printf '%s\n' "$config" | awk '$1 == "pubkeyauthentication" { print $2 }')" yes
}

check_ufw() {
  local status cidr missing=0
  if ! status=$(ufw status 2>/dev/null); then
    fail "ufw status is readable"
    return
  fi
  case "$status" in
    *"Status: active"*) pass "ufw is active" ;;
    *) fail "ufw is active" ;;
  esac
  if [ -z "${admin_ssh_cidrs:-}" ]; then
    warn "ADMIN_SSH_CIDR is unset; skipping the SSH rule check"
  fi
  for cidr in ${admin_ssh_cidrs:-}; do
    if printf '%s\n' "$status" | grep -Fq "$cidr" ||
      { [ "${cidr##*/}" = 32 ] && printf '%s\n' "$status" | grep -Fq "${cidr%/32}"; }; then
      pass "ufw allows SSH from $cidr"
    else
      fail "ufw allows SSH from $cidr"
    fi
  done
  [ "${#cloudflare_cidrs[@]}" -gt 0 ] || return
  for cidr in "${cloudflare_cidrs[@]}"; do
    if ! printf '%s\n' "$status" | grep -Fq "$cidr"; then
      fail "ufw allows Cloudflare range $cidr"
      missing=$((missing + 1))
    fi
  done
  if [ "$missing" -eq 0 ]; then
    pass "ufw allows all ${#cloudflare_cidrs[@]} Cloudflare ranges on 80/443"
  fi
}

check_unattended_upgrades() {
  if grep -Eq '^APT::Periodic::Unattended-Upgrade "1";' /etc/apt/apt.conf.d/20auto-upgrades 2>/dev/null; then
    pass "periodic unattended upgrades are enabled"
  else
    fail "periodic unattended upgrades are enabled"
  fi
  expect_value "unattended-upgrades is enabled at boot" \
    "$(systemctl is-enabled unattended-upgrades 2>/dev/null || true)" enabled
}

check_docker() {
  if cmp -s "$daemon_config" /etc/docker/daemon.json; then
    pass "Docker daemon log limits and no-new-privileges are installed"
  else
    fail "Docker daemon configuration matches provision/docker-daemon.json"
  fi
  expect_value "Docker is enabled at boot" "$(systemctl is-enabled docker 2>/dev/null || true)" enabled
  if command -v docker >/dev/null 2>&1; then
    expect_value "Docker logging driver is local" \
      "$(docker info --format '{{.LoggingDriver}}' 2>/dev/null || true)" local
  else
    fail "docker is installed"
  fi
}

check_directories() {
  local path mode
  for path in "$deploy_root" /etc/backendrelay /var/lib/backendrelay \
    /var/lib/backendrelay/deploy /var/lib/backendrelay/secrets-backups; do
    if [ ! -d "$path" ]; then
      fail "$path is missing"
      continue
    fi
    pass "$path exists"
  done
  for path in /etc/backendrelay /var/lib/backendrelay /var/lib/backendrelay/deploy \
    /var/lib/backendrelay/secrets-backups; do
    [ -d "$path" ] || continue
    mode=$(stat -c '%a' "$path")
    expect_value "$path is root-only (0700)" "$mode" 700
  done
}

check_time() {
  expect_value "host timezone is UTC" "$(timedatectl show -p Timezone --value 2>/dev/null || true)" UTC
  expect_value "NTP is enabled" "$(timedatectl show -p NTP --value 2>/dev/null || true)" yes
}

run_checks() {
  load_cloudflare_cidrs
  check_ssh
  check_ufw
  check_unattended_upgrades
  check_docker
  check_directories
  check_time
  if command -v age >/dev/null 2>&1; then
    pass "age is installed for the secrets backup"
  else
    fail "age is installed (needed for the server_secrets backup)"
  fi
  if ! command -v cosign >/dev/null 2>&1; then
    warn "cosign is not installed; deploy.sh verifies image signatures and needs it (see README.md)"
  fi
  if [ -e /var/run/reboot-required ]; then
    warn "a reboot is pending; schedule it with the drain procedure in docs/operations.md"
  fi
  if [ "$failures" -ne 0 ]; then
    die "$failures check(s) failed"
  fi
  log "All host checks passed"
}

if [ "$mode" = check ]; then
  run_checks
  exit 0
fi

log "Provisioning Ubuntu 24.04 ($(hostname)) for BackendRelay"
load_cloudflare_cidrs
install_packages
configure_host
configure_ssh
configure_ufw
configure_docker
configure_unattended_upgrades
log "Host provisioning complete"
log "Next: Cloudflare DNS/WAF, R2, PostgreSQL, email and GHCR pull access in provision/README.md"
log "Then run this script again with --check"
