#!/usr/bin/env bash
set -Eeuo pipefail

# Run the upstream Core, Admin, Platform, React, React Native, Webhooks, CLI,
# and resumable-stream suites against the native server.
# This intentionally uses the hermetic test Compose stack, so a passing run proves the
# official clients can create an app, transact, query, and manage schema on the
# Rust implementation without contacting api.instantdb.com.
#
# With --published-version, only the Core and Admin e2e suites run, against the
# published npm tarballs of that exact version. The workspace is never built in
# that mode, so a passing run proves the shipped SDK, not client/packages/*/dist.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export COMPOSE_PROJECT_NAME=instant-sdk-e2e-$$-$RANDOM
available_ports=()
while IFS= read -r acceptance_port; do
  available_ports+=("$acceptance_port")
done < <(python3 - <<'PY'
import socket

sockets = []
for _ in range(3):
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sockets.append(sock)
for sock in sockets:
    print(sock.getsockname()[1])
for sock in sockets:
    sock.close()
PY
)
export INSTANT_SERVER_PORT=${available_ports[0]}
export INSTANT_POSTGRES_PORT=${available_ports[1]}
export INSTANT_S3_TEST_PORT=${available_ports[2]}
server_origin="http://127.0.0.1:$INSTANT_SERVER_PORT"
s3_origin="http://127.0.0.1:$INSTANT_S3_TEST_PORT"
compose=(docker compose -f "$repo_root/self-hosting/docker-compose.test.yml")
suite=full
server_image=${INSTANT_SERVER_IMAGE:-}
published_version=
published_scope=@backendrelay
core_tarball=
admin_tarball=
compat_root=

if command -v mise >/dev/null 2>&1; then
  pnpm_cmd=(mise exec node@24 pnpm@10.34.9 -- pnpm)
  npm_cmd=(mise exec node@24 -- npm)
else
  pnpm_cmd=(pnpm)
  npm_cmd=(npm)
fi

usage() {
  cat <<'EOF'
usage: run-official-sdk-e2e.sh [--suite smoke|full]
       run-official-sdk-e2e.sh --published-version VERSION [--scope SCOPE]
                               [--core-tarball FILE --admin-tarball FILE]

  smoke  Build the clients and run the smallest official Core browser test.
  full   Run every official SDK wrapper (the default).

  --published-version VERSION
         Run only the Core and Admin e2e suites against the published npm
         tarballs of this exact version. This never builds the workspace.
  --scope SCOPE
         npm scope for the pinned tarballs (default @backendrelay).
  --core-tarball FILE, --admin-tarball FILE
         Use already-fetched tarballs instead of asking npm for them.

Set INSTANT_SERVER_IMAGE to a canonical digest-only image reference. The harness
pulls that exact reference and disables Compose builds; without it, the local
Rust image is built from server-rs as before.
EOF
}

while (($#)); do
  case "$1" in
    --suite)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      suite=$2
      shift 2
      ;;
    --published-version)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      published_version=$2
      shift 2
      ;;
    --scope)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      published_scope=$2
      shift 2
      ;;
    --core-tarball)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      core_tarball=$2
      shift 2
      ;;
    --admin-tarball)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      admin_tarball=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

case "$suite" in
  smoke|full) ;;
  *) usage >&2; exit 2 ;;
esac

if [[ -n $published_version ]]; then
  [[ $published_version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || {
    echo "--published-version must be an exact semver version, found: $published_version" >&2
    exit 2
  }
  [[ $published_scope =~ ^@[a-z0-9][a-z0-9._-]*$ ]] || {
    echo "--scope must be an npm scope such as @backendrelay, found: $published_scope" >&2
    exit 2
  }
fi
if [[ -n $core_tarball || -n $admin_tarball ]] && [[ -z $published_version ]]; then
  echo "--core-tarball and --admin-tarball need --published-version" >&2
  exit 2
fi

if [[ -n $server_image ]]; then
  "$repo_root/self-hosting/verify-release-images.sh" --shape-only "$server_image"
fi

run_bounded() {
  local label=$1 duration=$2
  shift 2
  echo "==> $label (limit: $duration)"
  if command -v timeout >/dev/null 2>&1; then
    timeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout --foreground --signal=TERM --kill-after=15s "$duration" "$@"
  else
    "$@"
  fi
}

fetch_published_tarball() {
  local package=$1 override=$2
  if [[ -n $override ]]; then
    [[ -f $override ]] || {
      echo "tarball does not exist: $override" >&2
      exit 1
    }
    local directory
    directory=$(cd "$(dirname "$override")" && pwd)
    printf '%s\n' "$directory/$(basename "$override")"
    return
  fi
  local pack_dir="$compat_root/packs"
  local archive="$pack_dir/${published_scope#@}-$package-$published_version.tgz"
  if [[ ! -f $archive ]]; then
    "${npm_cmd[@]}" pack "$published_scope/$package@$published_version" \
      --pack-destination "$pack_dir" >/dev/null
  fi
  [[ -f $archive ]] || {
    echo "npm pack did not create $archive" >&2
    exit 1
  }
  printf '%s\n' "$archive"
}

prepare_published_compat() {
  mkdir -p "$repo_root/client/packages/core/node_modules"
  compat_root=$(mktemp -d "$repo_root/client/packages/core/node_modules/published-sdk-compat.XXXXXX")
  mkdir -p "$compat_root/packs"

  local core_pack admin_pack
  core_pack=$(fetch_published_tarball core "$core_tarball")
  admin_pack=$(fetch_published_tarball admin "$admin_tarball")

  cat >"$compat_root/package.json" <<EOF
{
  "name": "backendrelay-published-sdk-compat",
  "private": true,
  "type": "module",
  "dependencies": {
    "$published_scope/core": "file:$core_pack",
    "$published_scope/admin": "file:$admin_pack"
  }
}
EOF

  # Installing the tarballs, not the workspace, is the point of this mode.
  run_bounded "install published SDK tarballs" "${INSTANT_SDK_E2E_INSTALL_TIMEOUT:-10m}" \
    "${npm_cmd[@]}" install --prefix "$compat_root" --no-audit --no-fund \
      --ignore-scripts --prefer-offline >/dev/null

  python3 - "$compat_root" "$published_scope" "$published_version" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
scope = sys.argv[2]
version = sys.argv[3]
for package in ("core", "admin"):
    installed = root / "node_modules" / scope / package
    manifest = installed / "package.json"
    if not manifest.is_file():
        raise SystemExit(f"published tarball did not install {scope}/{package}")
    actual = json.loads(manifest.read_text())["version"]
    if actual != version:
        raise SystemExit(
            f"{scope}/{package} installed {actual}, expected the exact pinned {version}"
        )
    if not (installed / "dist" / "esm" / "index.js").is_file():
        raise SystemExit(f"{scope}/{package} tarball has no dist/esm build")
PY

  python3 - "$repo_root" "$compat_root" "$published_scope" <<'PY'
import pathlib
import re
import shutil
import sys

repo = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2])
scope = sys.argv[3]

core_source = repo / "client" / "packages" / "core" / "__tests__" / "src"
core_target = root / "tests" / "core"
(core_target / "utils").mkdir(parents=True)
for source in sorted(core_source.glob("*.e2e.test.ts")):
    shutil.copy2(source, core_target / source.name)
for name in ("e2e.ts", "msw.ts"):
    shutil.copy2(core_source / "utils" / name, core_target / "utils" / name)

admin_source = repo / "client" / "packages" / "admin" / "src" / "live.e2e.test.ts"
admin_target = root / "tests" / "admin" / "live.e2e.test.ts"
admin_target.parent.mkdir(parents=True)
shutil.copy2(admin_source, admin_target)

source_import = re.compile(
    r"""from (['"])(?:\.\./)+src(?:/([A-Za-z0-9_]+))?\1"""
)


def rewrite_core(path):
    depth = len(path.relative_to(root).parts) - 1
    prefix = "../" * depth
    package_root = f"{prefix}node_modules/{scope}/core/dist/esm"

    def replacement(match):
        quote = match.group(1)
        module = match.group(2)
        target = f"{package_root}/{module}.js" if module else f"{package_root}/index.js"
        return f"from {quote}{target}{quote}"

    text = path.read_text()
    rewritten = source_import.sub(replacement, text)
    if rewritten == text and "../../src" in text:
        raise SystemExit(f"could not redirect the published package import in {path}")
    path.write_text(rewritten)


for path in sorted(core_target.rglob("*.ts")):
    rewrite_core(path)

admin_text = admin_target.read_text()
admin_import = f"from '../../node_modules/{scope}/admin/dist/esm/index.js'"
rewritten_admin = admin_text.replace("from './index.ts'", admin_import)
if rewritten_admin == admin_text:
    raise SystemExit(f"could not redirect the published package import in {admin_target}")
admin_target.write_text(rewritten_admin)
PY

  cat >"$compat_root/vitest.config.mjs" <<'EOF'
import { playwright } from '@vitest/browser-playwright';
import { defineConfig } from 'vitest/config';

const apiUrl = process.env.INSTANT_E2E_API_URL ?? '';
const chromiumExecutablePath =
  process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;

export default defineConfig({
  root: import.meta.dirname,
  define: {
    __DEV_LOCAL_PORT__: 0,
    __E2E_API_URL__: JSON.stringify(apiUrl),
    __BYOP_APP_ID__: '""',
    __BYOP_MUTATOR_URL__: '""',
    __BYOP_MUTATOR_TOKEN__: '""',
  },
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: 'core-e2e',
          include: ['tests/core/**/*.e2e.test.ts'],
          expect: { poll: { timeout: 10_000 } },
          browser: {
            enabled: true,
            provider: playwright({
              launchOptions: chromiumExecutablePath
                ? { executablePath: chromiumExecutablePath }
                : undefined,
            }),
            screenshotFailures: false,
            headless: true,
            instances: [{ browser: 'chromium' }],
          },
        },
      },
      {
        extends: true,
        test: {
          name: 'admin-e2e',
          include: ['tests/admin/**/*.e2e.test.ts'],
          environment: 'node',
        },
      },
    ],
  },
});
EOF
}

run_published_compat_suites() {
  prepare_published_compat

  run_bounded "published $published_scope/core@$published_version Core e2e" \
    "${INSTANT_SDK_E2E_SUITE_TIMEOUT:-10m}" \
    env INSTANT_E2E_API_URL="$server_origin" \
      "${pnpm_cmd[@]}" --filter @instantdb/core exec vitest run \
        --config "$compat_root/vitest.config.mjs" --project core-e2e
  run_bounded "published $published_scope/admin@$published_version Admin e2e" \
    "${INSTANT_SDK_E2E_SUITE_TIMEOUT:-10m}" \
    env INSTANT_ADMIN_LIVE_E2E=1 INSTANT_API_URI="$server_origin" \
      "${pnpm_cmd[@]}" --filter @instantdb/admin exec vitest run \
        --config "$compat_root/vitest.config.mjs" --project admin-e2e
}

cleanup() {
  if [[ -n $compat_root ]]; then
    case "$compat_root" in
      "$repo_root"/client/packages/core/node_modules/published-sdk-compat.*)
        rm -rf -- "$compat_root" ;;
      *)
        echo "refusing to remove unexpected compatibility path: $compat_root" >&2 ;;
    esac
  fi
  "${compose[@]}" down --volumes --remove-orphans
}
trap cleanup EXIT

"${compose[@]}" down --volumes --remove-orphans

if [[ -n $server_image ]]; then
  run_bounded "pull candidate Rust server image" "${INSTANT_SDK_E2E_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull server
else
  run_bounded "build local Rust server image" "${INSTANT_SDK_E2E_BUILD_TIMEOUT:-20m}" \
    "${compose[@]}" build server
fi

if [[ -n ${INSTANT_POSTGRES_IMAGE:-} ]]; then
  run_bounded "pull pinned PostgreSQL image" "${INSTANT_SDK_E2E_PULL_TIMEOUT:-10m}" \
    "${compose[@]}" pull postgres
else
  run_bounded "build local PostgreSQL image" "${INSTANT_SDK_E2E_BUILD_TIMEOUT:-20m}" \
    "${compose[@]}" build postgres
fi

S3_PUBLIC_ENDPOINT=$s3_origin "${compose[@]}" up -d --no-build \
  postgres s3 createbuckets
postgres_ready_checks=0
for _ in $(seq 1 60); do
  if "${compose[@]}" exec -T postgres pg_isready -U postgres -d instant >/dev/null 2>&1; then
    ((postgres_ready_checks += 1))
    if ((postgres_ready_checks >= 2)); then
      break
    fi
  else
    postgres_ready_checks=0
  fi
  sleep 1
done
if ((postgres_ready_checks < 2)); then
  echo 'PostgreSQL did not become ready within 60 seconds' >&2
  exit 1
fi
run_bounded "migrate SDK fixture database" "${INSTANT_SDK_E2E_MIGRATE_TIMEOUT:-5m}" \
  env S3_PUBLIC_ENDPOINT=$s3_origin \
    "${compose[@]}" run --rm --no-deps server migrate up
S3_PUBLIC_ENDPOINT=$s3_origin "${compose[@]}" up -d --no-build server

for _ in $(seq 1 90); do
  if curl --silent --show-error --fail --max-time 2 \
      -X POST "$server_origin/dash/apps/ephemeral" \
      -H 'content-type: application/json' \
      --data '{"title":"sdk-e2e-readiness"}' >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

if ! curl --silent --show-error --fail --max-time 2 \
    -X POST "$server_origin/dash/apps/ephemeral" \
    -H 'content-type: application/json' \
    --data '{"title":"sdk-e2e-readiness"}' >/dev/null; then
  echo 'Rust server did not become ready within 180 seconds' >&2
  "${compose[@]}" logs server >&2 || true
  exit 1
fi

if [[ ${pnpm_cmd[0]} == pnpm ]]; then
  command -v pnpm >/dev/null 2>&1 || {
    echo "required command is not installed: pnpm" >&2
    exit 1
  }
fi
if [[ -n $published_version && ${npm_cmd[0]} == npm ]]; then
  command -v npm >/dev/null 2>&1 || {
    echo "required command is not installed: npm" >&2
    exit 1
  }
fi
pushd "$repo_root/client" >/dev/null
run_bounded "install official SDK workspace dependencies" "${INSTANT_SDK_E2E_INSTALL_TIMEOUT:-10m}" \
  "${pnpm_cmd[@]}" install --frozen-lockfile
if [[ -n "${PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH:-}" ]]; then
  [[ -x "$PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH" ]] || {
    echo "PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH must name an executable browser" >&2
    exit 1
  }
else
  run_bounded "install Playwright Chromium" "${INSTANT_SDK_E2E_PLAYWRIGHT_TIMEOUT:-10m}" \
    "${pnpm_cmd[@]}" --filter @instantdb/core exec playwright install chromium
fi
if [[ -n $published_version ]]; then
  run_published_compat_suites
  popd >/dev/null
  echo "Published SDK compatibility lane passed for $published_scope/*@$published_version"
  exit 0
fi
"${pnpm_cmd[@]}" run build-packages
if [[ "$suite" == smoke ]]; then
  run_bounded "official Core browser smoke" "${INSTANT_SDK_E2E_SMOKE_TIMEOUT:-5m}" \
    env INSTANT_E2E_API_URL=$server_origin \
      "${pnpm_cmd[@]}" --filter @instantdb/core exec vitest run \
        --project e2e __tests__/src/simple.e2e.test.ts
  popd >/dev/null
  echo "Official SDK image smoke passed"
  exit 0
fi

run_bounded "CLI unit tests" 10m \
  "${pnpm_cmd[@]}" --filter instant-cli exec vitest run --project unit
run_bounded "Core unit tests" 10m \
  "${pnpm_cmd[@]}" --filter @instantdb/core exec vitest run --project node
for package in \
  @instantdb/admin \
  @instantdb/components \
  @instantdb/expo-sqlite \
  @instantdb/platform \
  @instantdb/react \
  @instantdb/react-native \
  @instantdb/react-native-mmkv \
  @instantdb/resumable-stream \
  @instantdb/webhooks
do
  run_bounded "$package unit and type tests" 10m \
    env INSTANT_API_URI=$server_origin "${pnpm_cmd[@]}" --filter "$package" test:ci
done
INSTANT_CLI_API_URI=$server_origin INSTANT_CLI_E2E_TIMEOUT_MS=60000 \
  "${pnpm_cmd[@]}" --filter instant-cli exec vitest run --project e2e
INSTANT_E2E_API_URL=$server_origin "${pnpm_cmd[@]}" --filter @instantdb/core test:e2e -- --run
INSTANT_API_URI=$server_origin "${pnpm_cmd[@]}" --filter @instantdb/resumable-stream test:ci -- --reporter=verbose
INSTANT_ADMIN_LIVE_E2E=1 INSTANT_API_URI=$server_origin \
  "${pnpm_cmd[@]}" --filter @instantdb/admin test:e2e -- --run
INSTANT_API_URI=$server_origin \
  "${pnpm_cmd[@]}" --filter @instantdb/platform test:e2e -- --run
INSTANT_REACT_LIVE_E2E=1 INSTANT_API_URI=$server_origin \
  "${pnpm_cmd[@]}" --filter @instantdb/react test:e2e -- --run
INSTANT_REACT_NATIVE_LIVE_E2E=1 INSTANT_API_URI=$server_origin \
  "${pnpm_cmd[@]}" --filter @instantdb/react-native test:e2e -- --run
INSTANT_WEBHOOKS_LIVE_E2E=1 INSTANT_API_URI=$server_origin \
  "${pnpm_cmd[@]}" --filter @instantdb/webhooks test:e2e -- --run
popd >/dev/null
