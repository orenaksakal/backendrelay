import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  OWN_SCOPE,
  UPSTREAM_SCOPE,
  resolvePinnedSdk,
} from './resolve-pinned-sdk.mjs';

function examples(packages) {
  const root = mkdtempSync(join(tmpdir(), 'sdk-compat-examples-'));
  for (const [name, manifest] of Object.entries(packages)) {
    const directory = join(root, name);
    mkdirSync(directory);
    writeFileSync(join(directory, 'package.json'), JSON.stringify(manifest));
  }
  return root;
}

test('prefers the BackendRelay scope when an example already uses it', (t) => {
  const root = examples({
    app: {
      dependencies: {
        '@instantdb/admin': '1.0.66',
        '@backendrelay/core': '1.1.0',
      },
    },
  });
  t.after(() => rmSync(root, { recursive: true, force: true }));

  assert.deepEqual(resolvePinnedSdk(root), {
    scope: OWN_SCOPE,
    version: '1.1.0',
    pins: ['@backendrelay/core@1.1.0'],
  });
});

test('uses upstream pins until the examples move scopes', (t) => {
  const root = examples({
    a: { dependencies: { '@instantdb/react': '1.0.66' } },
    b: { devDependencies: { '@instantdb/admin': '1.0.66' } },
  });
  t.after(() => rmSync(root, { recursive: true, force: true }));

  assert.deepEqual(resolvePinnedSdk(root), {
    scope: UPSTREAM_SCOPE,
    version: '1.0.66',
    pins: ['@instantdb/react@1.0.66', '@instantdb/admin@1.0.66'],
  });
});

test('rejects ranges and disagreeing pins', (t) => {
  const ranged = examples({
    app: { dependencies: { '@backendrelay/core': '^1.1.0' } },
  });
  const split = examples({
    a: { dependencies: { '@backendrelay/core': '1.1.0' } },
    b: { dependencies: { '@backendrelay/admin': '1.1.1' } },
  });
  t.after(() => {
    rmSync(ranged, { recursive: true, force: true });
    rmSync(split, { recursive: true, force: true });
  });

  assert.throws(() => resolvePinnedSdk(ranged), /must pin an exact version/);
  assert.throws(() => resolvePinnedSdk(split), /more than one SDK version/);
});
