import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, symlinkSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const verifier = fileURLToPath(new URL('./verify-release-images.sh', import.meta.url));
const server = `ghcr.io/example/instantdb-server@sha256:${'a'.repeat(64)}`;
const dashboard = `ghcr.io/example/instantdb-dashboard@sha256:${'b'.repeat(64)}`;

// Keep the verifier's PATH limited to the mocks while still giving it the few
// shell utilities the script uses.
function linkCommand(bin, name) {
  const resolved = spawnSync('/bin/sh', ['-c', `command -v ${name}`], { encoding: 'utf8' });
  assert.equal(resolved.status, 0, `${name} is required to run the verifier test`);
  symlinkSync(resolved.stdout.trim(), join(bin, name));
}

test('release verification fails closed and verifies signatures, platforms and attestations', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'instant-signature-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const bin = join(root, 'bin');
  mkdirSync(bin);
  for (const name of ['dirname', 'grep', 'wc']) linkCommand(bin, name);
  const cosignLog = join(root, 'cosign.log');
  const dockerLog = join(root, 'docker.log');
  const key = join(root, 'cosign.pub');
  writeFileSync(key, 'release public key\n');
  const run = (args = [server, dashboard], env = {}) => spawnSync('/bin/sh', [verifier, ...args], {
    encoding: 'utf8', env: { PATH: bin, MOCK_LOG: cosignLog, MOCK_DOCKER_LOG: dockerLog, ...env },
  });
  const rejects = (result, message) => {
    assert.ifError(result.error);
    assert.equal(typeof result.status, 'number');
    assert.notEqual(result.status, 0, message);
  };

  rejects(run(undefined, { COSIGN_PUBLIC_KEY: key }), 'missing cosign must fail');
  assert.equal(run(['--shape-only', server, dashboard]).status, 0);
  rejects(run(['--shape-only', 'ghcr.io/example/server:latest']), 'syntax-only rejects tags');
  rejects(run([]), 'no images must fail');

  writeFileSync(join(bin, 'cosign'), `#!/bin/sh
printf '%s\\n' "$*" >> "$MOCK_LOG"
case "$*" in *instantdb-dashboard*) exit "\${MOCK_DASHBOARD_STATUS:-0}";; esac
`, { mode: 0o755 });

  rejects(run(undefined, { COSIGN_PUBLIC_KEY: join(root, 'missing.pub') }), 'missing trusted key must fail');
  rejects(run(undefined, { COSIGN_PUBLIC_KEY: key }), 'missing docker must fail');
  for (const value of ['false', 'auto', 'invalid']) {
    rejects(run(undefined, { COSIGN_PUBLIC_KEY: key, INSTANT_VERIFY_RELEASE_SIGNATURES: value }), `${value} must not bypass verification`);
  }
  rejects(run([server, 'ghcr.io/example/dashboard:latest'], { COSIGN_PUBLIC_KEY: key }), 'validate every reference before invoking cosign');

  writeFileSync(join(bin, 'docker'), `#!/bin/sh
printf '%s\\n' "$*" >> "$MOCK_DOCKER_LOG"
case "$*" in
  *'{{json .SBOM}}'*)
    if [ "\${MOCK_SBOM:-ok}" = ok ]; then
      printf '%s\\n' '{"linux/amd64":{"SPDXID":"SPDXRef-DOCUMENT"},"linux/arm64":{"SPDXID":"SPDXRef-DOCUMENT"}}'
    else
      printf '%s\\n' '{}'
    fi
    ;;
  *'{{json .Provenance}}'*)
    if [ "\${MOCK_PROVENANCE:-ok}" = ok ]; then
      printf '%s\\n' '{"linux/amd64":{"buildType":"slsa"},"linux/arm64":{"buildType":"slsa"}}'
    else
      printf '%s\\n' '{}'
    fi
    ;;
  *)
    if [ "\${MOCK_MANIFEST:-ok}" = ok ]; then
      printf 'Manifests:\\n  Platform: linux/amd64\\n  Platform: linux/arm64\\n'
    else
      printf 'Manifests:\\n  Platform: linux/amd64\\n'
    fi
    ;;
esac
exit "\${MOCK_DOCKER_STATUS:-0}"
`, { mode: 0o755 });

  const keyEnv = { COSIGN_PUBLIC_KEY: key };
  assert.equal(run(undefined, keyEnv).status, 0);
  assert.equal(readFileSync(cosignLog, 'utf8'), [server, dashboard].map((ref) =>
    `verify --key ${key} --insecure-ignore-tlog=true ${ref}\n`).join(''));
  assert.match(readFileSync(dockerLog, 'utf8'), /imagetools inspect .*instantdb-server/);
  assert.match(readFileSync(dockerLog, 'utf8'), /imagetools inspect .*instantdb-dashboard/);

  writeFileSync(dockerLog, '');
  rejects(run(undefined, { ...keyEnv, MOCK_MANIFEST: 'amd64-only' }), 'both architectures must be present');
  rejects(run(undefined, { ...keyEnv, MOCK_SBOM: 'missing' }), 'SBOM attestations must be present');
  rejects(run(undefined, { ...keyEnv, MOCK_PROVENANCE: 'missing' }), 'provenance attestations must be present');
  rejects(run(undefined, { ...keyEnv, MOCK_DOCKER_STATUS: '42' }), 'docker inspection failure must propagate');
  rejects(run(undefined, { ...keyEnv, MOCK_DASHBOARD_STATUS: '42' }), 'second image signature failure must propagate');
});
