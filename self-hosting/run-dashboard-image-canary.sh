#!/usr/bin/env bash
set -Eeuo pipefail

# Proves that an exact published dashboard image is non-root, fails closed
# without runtime configuration, serves the requested self-hosted API URI, and
# routes dashboard documents and executable assets through the public-site gateway.

: "${INSTANT_DASHBOARD_IMAGE:?set the exact dashboard image digest}"
"$(dirname "$0")/verify-release-images.sh" --shape-only "$INSTANT_DASHBOARD_IMAGE"

backend_url=${INSTANT_DASHBOARD_CANARY_BACKEND_URL:-https://api.dashboard-canary.invalid}
pull_timeout=${INSTANT_DASHBOARD_CANARY_PULL_TIMEOUT:-10m}
startup_seconds=${INSTANT_DASHBOARD_CANARY_STARTUP_SECONDS:-120}
[[ $startup_seconds =~ ^[1-9][0-9]*$ ]] || {
  echo 'INSTANT_DASHBOARD_CANARY_STARTUP_SECONDS must be a positive integer' >&2
  exit 1
}
container_name="instant-dashboard-canary-$$"
run_root=$(mktemp -d "${TMPDIR:-/tmp}/instant-dashboard-canary.XXXXXX")
invalid_log="$run_root/invalid.log"
runtime_config="$run_root/publicSelfHostedVariables.js"
container_started=false

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ "$container_started" == true ]]; then
    if ((status != 0)); then
      echo 'Last dashboard image log lines:' >&2
      docker logs --tail 100 "$container_name" 2>&1 \
        | sed -E 's#(https?://)[^/@[:space:]]+@#\1[REDACTED]@#g' >&2 || true
    fi
    docker rm --force "$container_name" >/dev/null 2>&1 || true
  fi
  case "$run_root" in
    */instant-dashboard-canary.*) rm -rf -- "$run_root" ;;
    *) echo "refusing to remove unexpected temporary path: $run_root" >&2 ;;
  esac
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in docker curl python3; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is not installed: $command" >&2
    exit 1
  }
done

run_bounded() {
  duration=$1
  shift
  if command -v timeout >/dev/null 2>&1; then
    timeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  else
    "$@"
  fi
}

python3 - "$backend_url" <<'PY'
import sys
from urllib.parse import urlparse

url = urlparse(sys.argv[1])
if url.scheme not in {"http", "https"} or not url.netloc:
    raise SystemExit("INSTANT_DASHBOARD_CANARY_BACKEND_URL must be an HTTP(S) URL")
if url.username is not None or url.password is not None:
    raise SystemExit("dashboard canary backend URL must not contain credentials")
if url.hostname == "api.instantdb.com":
    raise SystemExit("dashboard canary must use a self-hosted test API URL")
PY

run_bounded "$pull_timeout" docker pull "$INSTANT_DASHBOARD_IMAGE"

test "$(docker image inspect --format '{{.Config.User}}' "$INSTANT_DASHBOARD_IMAGE")" = nextjs
docker run --rm --entrypoint sh "$INSTANT_DASHBOARD_IMAGE" -ec '
  for path in \
    /app/node_modules \
    /app/packages \
    /app/www/node_modules \
    /app/www/.output/server/index.mjs \
    /app/www/dist-site/server/entry.mjs \
    /app/www/public \
  do
    test -e "$path"
  done
'

set +e
run_bounded 30s docker run --rm "$INSTANT_DASHBOARD_IMAGE" >"$invalid_log" 2>&1
invalid_status=$?
set -e
if ((invalid_status == 0 || invalid_status == 124)); then
  echo 'dashboard image did not fail closed without a backend URL' >&2
  exit 1
fi
grep -Fq 'INSTANT_API_URI or INSTANT_BACKEND_URL must be a valid HTTP(S) URL.' \
  "$invalid_log"

dashboard_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)

docker run --detach --name "$container_name" \
  --env INSTANT_BACKEND_URL="$backend_url" \
  --publish "127.0.0.1:$dashboard_port:3000" \
  "$INSTANT_DASHBOARD_IMAGE" >/dev/null
container_started=true

ready=false
for ((attempt = 0; attempt < startup_seconds; attempt += 2)); do
  if ! docker inspect --format '{{.State.Running}}' "$container_name" \
    | grep -Fxq true; then
    echo 'dashboard candidate exited before readiness' >&2
    exit 1
  fi
  if curl --silent --show-error --fail --max-time 2 \
    "http://127.0.0.1:$dashboard_port/publicSelfHostedVariables.js" \
    --output "$runtime_config"; then
    ready=true
    break
  fi
  sleep 2
done
[[ "$ready" == true ]] || {
  echo "dashboard candidate did not become ready within ${startup_seconds}s" >&2
  exit 1
}

python3 - "$runtime_config" "$backend_url" <<'PY'
import json
import pathlib
import sys

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").strip()
prefix = "window.__instantConfig = "
suffix = ";"
if not source.startswith(prefix) or not source.endswith(suffix):
    raise SystemExit("dashboard runtime config has an unexpected envelope")
config = json.loads(source[len(prefix):-len(suffix)])
if config != {"apiURI": sys.argv[2]}:
    raise SystemExit("dashboard runtime config mismatch")
if "api.instantdb.com" in source:
    raise SystemExit("dashboard runtime config leaked the Instant Cloud API origin")
PY

curl --silent --show-error --fail --max-time 5 \
  "http://127.0.0.1:$dashboard_port/" >/dev/null

python3 - "http://127.0.0.1:$dashboard_port" <<'PY'
import gzip
import io
import re
import sys
from html.parser import HTMLParser
from urllib.error import HTTPError, URLError
from urllib.parse import urljoin, urlsplit
from urllib.request import Request, urlopen

origin = sys.argv[1]
document_url = origin + "/dash"
maximum_bytes = 8 * 1024 * 1024

def read_response(url):
    request = Request(url, headers={"Accept-Encoding": "identity", "Referer": document_url})
    try:
        with urlopen(request, timeout=10) as response:
            # Parse the media type independently of charset and other parameters.
            media_type = response.headers.get_content_type()
            content = response.read(maximum_bytes + 1)
            if len(content) > maximum_bytes:
                raise SystemExit("dashboard asset exceeded the canary size bound")
            encoding = response.headers.get("Content-Encoding", "").strip().lower()
            if encoding == "gzip":
                with gzip.GzipFile(fileobj=io.BytesIO(content)) as compressed:
                    content = compressed.read(maximum_bytes + 1)
            elif encoding not in {"", "identity"}:
                raise SystemExit("dashboard asset returned unsupported content encoding")
            if len(content) > maximum_bytes:
                raise SystemExit("dashboard asset exceeded the canary size bound")
            return media_type, content
    except (HTTPError, URLError, TimeoutError, OSError) as error:
        raise SystemExit("dashboard asset request failed: " + urlsplit(url).path) from error

class Assets(HTMLParser):
    def __init__(self):
        super().__init__()
        self.assets = {}

    def handle_starttag(self, tag, attributes):
        attributes = dict(attributes)
        kind = None
        href = None
        if tag == "script" and attributes.get("src"):
            href, kind = attributes["src"], "javascript"
        elif tag == "link":
            rel = attributes.get("rel", "").split()
            if "modulepreload" in rel:
                href, kind = attributes.get("href"), "javascript"
            elif "stylesheet" in rel:
                href, kind = attributes.get("href"), "css"
        if not href:
            return
        url = urljoin(document_url, href)
        parsed = urlsplit(url)
        if parsed.path.startswith("/_build/"):
            if (parsed.scheme, parsed.netloc) != (urlsplit(origin).scheme, urlsplit(origin).netloc):
                raise SystemExit("dashboard build asset unexpectedly points to another origin")
            self.assets[url] = kind

media_type, document = read_response(document_url)
if media_type != "text/html":
    raise SystemExit("dashboard document did not return HTML")
parser = Assets()
parser.feed(document.decode("utf-8"))
if set(parser.assets.values()) != {"javascript", "css"}:
    raise SystemExit("dashboard HTML must reference compiled JavaScript and CSS")
if len(parser.assets) > 32:
    raise SystemExit("dashboard asset count exceeded the canary bound")

for url, kind in parser.assets.items():
    media_type, content = read_response(url)
    allowed = {"text/css"} if kind == "css" else {"text/javascript", "application/javascript"}
    if media_type not in allowed:
        raise SystemExit("dashboard " + kind + " asset returned " + media_type + ": " + urlsplit(url).path)
    if not content.strip() or re.match(br"\s*(?:<!doctype\s+html|<html\b|<head\b|<body\b)", content, re.IGNORECASE):
        raise SystemExit("dashboard asset returned empty or HTML content: " + urlsplit(url).path)

print("Dashboard JavaScript and CSS asset checks passed")
PY

echo "Dashboard image canary passed"
