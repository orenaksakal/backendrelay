import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  PACKAGES,
  REPOSITORY,
  publishedName,
  readVersion,
  rewriteManifest,
  rewriteScope,
} from './publish_packages.mjs';

const clientRoot = dirname(dirname(fileURLToPath(import.meta.url)));

test('the publish list covers exactly the packages on disk', () => {
  const onDisk = readdirSync(join(clientRoot, 'packages'), {
    withFileTypes: true,
  })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();
  assert.deepEqual([...PACKAGES].sort(), onDisk);
});

test('published names use the BackendRelay scope', () => {
  assert.equal(publishedName('core'), '@backendrelay/core');
  assert.equal(publishedName('react-native'), '@backendrelay/react-native');
});

test('the shared version is a semver release', () => {
  assert.match(readVersion(), /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/);
});

test('a staged manifest is renamed and pointed at this repository', () => {
  const source = JSON.stringify({
    name: '@instantdb/core',
    version: '0.0.0',
    homepage: 'https://github.com/instantdb/instant/tree/main/client/packages/core',
    repository: {
      type: 'git',
      url: 'git+https://github.com/instantdb/instant.git',
      directory: 'client/packages/core',
    },
    dependencies: {
      '@instantdb/version': 'workspace:*',
      '@instantdb/eventsource': '4.1.0-2',
      uuid: '^11.1.0',
    },
    peerDependencies: { '@instantdb/admin': 'workspace:^' },
    devDependencies: {
      '@instantdb/react-common': 'workspace:*',
      typescript: '^5.9.3',
    },
  });

  const manifest = rewriteManifest(source, 'core', '1.1.0');

  assert.equal(manifest.name, '@backendrelay/core');
  assert.equal(manifest.version, '1.1.0');
  assert.equal(manifest.homepage, `${REPOSITORY}/tree/main/client/packages/core`);
  assert.deepEqual(manifest.repository, {
    type: 'git',
    url: `git+${REPOSITORY}.git`,
    directory: 'client/packages/core',
  });
  assert.deepEqual(manifest.dependencies, {
    // Siblings come from the same publish run, so the workspace range becomes
    // the version being published.
    '@backendrelay/version': '^1.1.0',
    // Not one of ours: it stays a normal dependency on upstream's package.
    '@instantdb/eventsource': '4.1.0-2',
    uuid: '^11.1.0',
  });
  assert.deepEqual(manifest.peerDependencies, {
    '@backendrelay/admin': '^1.1.0',
  });
  assert.deepEqual(manifest.devDependencies, {
    '@backendrelay/react-common': '^1.1.0',
    typescript: '^5.9.3',
  });
});

test('build output moves sibling imports but not upstream packages', () => {
  const source = [
    "import { init } from '@instantdb/core';",
    "import { useQuery } from '@instantdb/react';",
    "import { common } from '@instantdb/react-common';",
    "import { EventSource } from '@instantdb/eventsource';",
  ].join('\n');

  assert.equal(
    rewriteScope(source),
    [
      "import { init } from '@backendrelay/core';",
      "import { useQuery } from '@backendrelay/react';",
      "import { common } from '@backendrelay/react-common';",
      "import { EventSource } from '@instantdb/eventsource';",
    ].join('\n'),
  );
});
