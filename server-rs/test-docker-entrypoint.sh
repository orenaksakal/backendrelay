#!/bin/sh
set -eu

cd "$(dirname "$0")"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/instant-entrypoint-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fake_server="$temporary/instant-server"
call_log="$temporary/calls"
proxy_log="$temporary/proxy-cidrs"
cat >"$fake_server" <<'FAKE_SERVER'
#!/bin/sh
set -eu
printf '<%s>\n' "$*" >>"$CALL_LOG"
if [ -n "${INSTANT_TRUSTED_PROXY_CIDRS:-}" ]; then
    printf '%s\n' "$INSTANT_TRUSTED_PROXY_CIDRS" >>"${PROXY_LOG:-/dev/null}"
fi
if [ "${1:-}" = generate-secrets ]; then
    printf "INSTANT_AEAD_KEYSET_JSON='test-aead'\n"
    printf "INSTANT_WEBHOOK_KEYSET_JSON='test-webhook'\n"
    printf "INSTANT_RATE_LIMIT_HMAC_KEY='test-rate-limit'\n"
fi
FAKE_SERVER
chmod +x "$fake_server"

run_entrypoint() {
    auto_migrate=$1
    shift
    : >"$call_log"
    env \
        CALL_LOG="$call_log" \
        PROXY_LOG="$proxy_log" \
        INSTANT_SERVER_BIN="$fake_server" \
        INSTANT_CONFIG_DIR="$temporary/config" \
        INSTANT_AEAD_KEYSET_JSON=test-aead \
        INSTANT_WEBHOOK_KEYSET_JSON=test-webhook \
        INSTANT_RATE_LIMIT_HMAC_KEY=test-rate-limit \
        INSTANT_AUTO_MIGRATE="$auto_migrate" \
        INSTANT_TRUSTED_PROXY_CIDRS= \
        ./docker-entrypoint.sh "$@"
}

run_entrypoint_with_proxy() {
    proxy_cidrs=$1
    proxy_host=$2
    : >"$call_log"
    : >"$proxy_log"
    env \
        PATH="$stub_bin:$PATH" \
        CALL_LOG="$call_log" \
        PROXY_LOG="$proxy_log" \
        INSTANT_SERVER_BIN="$fake_server" \
        INSTANT_CONFIG_DIR="$temporary/config" \
        INSTANT_AEAD_KEYSET_JSON=test-aead \
        INSTANT_WEBHOOK_KEYSET_JSON=test-webhook \
        INSTANT_RATE_LIMIT_HMAC_KEY=test-rate-limit \
        INSTANT_TRUSTED_PROXY_CIDRS="$proxy_cidrs" \
        INSTANT_TRUSTED_PROXY_HOST="$proxy_host" \
        ./docker-entrypoint.sh serve
}

run_entrypoint true serve
test "$(cat "$call_log")" = '<serve>'

run_entrypoint false serve
test "$(cat "$call_log")" = '<serve>'

run_entrypoint true
test "$(cat "$call_log")" = '<>'

run_entrypoint false migrate up
test "$(cat "$call_log")" = '<migrate up>'

: >"$call_log"
rm -rf "$temporary/config"
env \
    CALL_LOG="$call_log" \
    INSTANT_SERVER_BIN="$fake_server" \
    INSTANT_CONFIG_DIR="$temporary/config" \
    INSTANT_TRUSTED_PROXY_CIDRS= \
    ./docker-entrypoint.sh migrate up
test "$(cat "$call_log")" = '<generate-secrets>
<migrate up>'

run_entrypoint true configure-byop \
    00000000-0000-0000-0000-000000000001
test "$(cat "$call_log")" = \
    '<configure-byop 00000000-0000-0000-0000-000000000001>'

if grep -Fq '<migrate up>' "$call_log"; then
    echo 'non-migration entrypoint path invoked migrate up' >&2
    exit 1
fi

stub_bin="$temporary/bin"
mkdir -p "$stub_bin"
cat >"$stub_bin/getent" <<'STUB_GETENT'
#!/bin/sh
[ "${1:-}" = ahosts ] || exit 1
case "${2:-}" in
    caddy)
        printf '10.1.2.3  STREAM caddy\n'
        printf '10.1.2.3  DGRAM caddy\n'
        printf 'fd00::5   STREAM caddy\n'
        ;;
    *) exit 2 ;;
esac
STUB_GETENT
chmod +x "$stub_bin/getent"

run_entrypoint_with_proxy auto caddy
test "$(cat "$call_log")" = '<serve>'
test "$(cat "$proxy_log")" = '10.1.2.3/32,fd00::5/128'

if run_entrypoint_with_proxy auto missing 2>"$temporary/warning"; then
    :
else
    echo 'entrypoint failed when the proxy host did not resolve' >&2
    exit 1
fi
test "$(cat "$call_log")" = '<serve>'
test ! -s "$proxy_log"
grep -Fq "starting without trusted proxies" "$temporary/warning"

run_entrypoint_with_proxy 10.0.0.0/8 unused
test "$(cat "$call_log")" = '<serve>'
test "$(cat "$proxy_log")" = '10.0.0.0/8'

echo 'Docker entrypoint trusts only the resolved proxy host in auto mode'
