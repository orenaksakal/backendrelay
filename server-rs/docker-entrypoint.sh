#!/bin/sh
set -eu

server_bin=${INSTANT_SERVER_BIN:-/usr/local/bin/instant-server}

# `auto` trusts only the address of the proxy container, never its whole Docker
# network. Resolve the service name at startup and pin each address as a host
# route (/32 or /128). Explicit CIDR lists remain untouched.
if [ "${INSTANT_TRUSTED_PROXY_CIDRS:-}" = auto ]; then
    proxy_host=${INSTANT_TRUSTED_PROXY_HOST:-caddy}
    cidrs=
    while read -r address _rest; do
        [ -n "$address" ] || continue
        case "$address" in
            *:*) candidate="$address/128" ;;
            *) candidate="$address/32" ;;
        esac
        case ",$cidrs," in
            *",$candidate,"*) continue ;;
        esac
        cidrs="${cidrs:+$cidrs,}$candidate"
    done <<EOF
$(getent ahosts "$proxy_host" 2>/dev/null || true)
EOF
    if [ -z "$cidrs" ]; then
        echo "No address found for trusted proxy host '$proxy_host'; starting without trusted proxies." >&2
        unset INSTANT_TRUSTED_PROXY_CIDRS
    else
        export INSTANT_TRUSTED_PROXY_CIDRS="$cidrs"
    fi
fi

case "${1:-serve}" in
serve|configure-byop|migrate)
    config_dir="${INSTANT_CONFIG_DIR:-/home/instant/config}"
    secrets_file="$config_dir/secrets.env"
    if [ -z "${INSTANT_AEAD_KEYSET_JSON:-}" ] \
        || [ -z "${INSTANT_WEBHOOK_KEYSET_JSON:-}" ] \
        || [ -z "${INSTANT_RATE_LIMIT_HMAC_KEY:-}" ]; then
        mkdir -p "$config_dir"
        if [ ! -s "$secrets_file" ]; then
            umask 077
            temporary="$secrets_file.tmp"
            "$server_bin" generate-secrets > "$temporary"
            mv "$temporary" "$secrets_file"
        fi
        set -a
        # Generated values are single-quoted JSON/Base64 assignments only.
        . "$secrets_file"
        set +a
    fi
    ;;
esac

exec "$server_bin" "$@"
