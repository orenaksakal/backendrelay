#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
origin=${1:-${R2_CORS_ORIGIN:-}}
if [ -z "$origin" ]; then
  echo "usage: $0 https://dashboard.example.com" >&2
  exit 2
fi
exec "$script_dir/r2-wrangler.sh" configure-cors "$origin"
