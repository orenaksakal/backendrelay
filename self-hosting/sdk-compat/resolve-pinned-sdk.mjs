#!/usr/bin/env node

// Resolves the exact SDK version that the kept example apps pin.
//
// The compatibility lane must test the tarball that apps actually install, not
// the workspace build. Applications that already moved to the BackendRelay
// scope win; otherwise the upstream @instantdb pins are used. Every kept
// example must agree on one scope and one exact version.

import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const scriptPath = fileURLToPath(import.meta.url);
const repoRoot = resolve(dirname(scriptPath), '..', '..');

export const OWN_SCOPE = '@backendrelay';
export const UPSTREAM_SCOPE = '@instantdb';

// The SDK packages that an app can pin. Other @instantdb packages, such as
// @instantdb/eventsource, are third-party dependencies and are ignored.
const SDK_PACKAGES = new Set([
  'admin',
  'cli',
  'components',
  'core',
  'expo-sqlite',
  'platform',
  'react',
  'react-common',
  'react-native',
  'react-native-mmkv',
  'resumable-stream',
  'version',
  'webhooks',
]);

const EXACT_VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

function scopedPackage(name) {
  for (const scope of [OWN_SCOPE, UPSTREAM_SCOPE]) {
    const prefix = `${scope}/`;
    if (name.startsWith(prefix) && SDK_PACKAGES.has(name.slice(prefix.length))) {
      return { scope, packageName: name.slice(prefix.length) };
    }
  }
  return undefined;
}

function readManifest(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

// Returns { scope, version, pins } where pins lists every exact
// `<scope>/<package>@<version>` declaration that contributed.
export function resolvePinnedSdk(examplesRoot = join(repoRoot, 'examples')) {
  const pins = [];
  const examples = readdirSync(examplesRoot, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => join(examplesRoot, entry.name, 'package.json'))
    .sort();

  for (const path of examples) {
    let manifest;
    try {
      manifest = readManifest(path);
    } catch (error) {
      if (error.code === 'ENOENT') continue;
      throw error;
    }
    for (const field of ['dependencies', 'devDependencies']) {
      for (const [name, range] of Object.entries(manifest[field] ?? {})) {
        const packageInfo = scopedPackage(name);
        if (!packageInfo) continue;
        if (!EXACT_VERSION.test(range)) {
          throw new Error(
            `${path}: ${name} must pin an exact version, found ${range}`,
          );
        }
        pins.push({ ...packageInfo, version: range, name, path });
      }
    }
  }

  if (pins.length === 0) {
    throw new Error(`no pinned SDK package found under ${examplesRoot}`);
  }

  const ownPins = pins.filter((pin) => pin.scope === OWN_SCOPE);
  const selected = ownPins.length > 0 ? ownPins : pins;
  const versions = new Set(selected.map((pin) => pin.version));
  if (versions.size > 1) {
    const details = selected
      .map((pin) => `${pin.name}@${pin.version} (${pin.path})`)
      .join('\n');
    throw new Error(
      `the examples pin more than one SDK version; publish one exact version:\n${details}`,
    );
  }

  return {
    scope: selected[0].scope,
    version: selected[0].version,
    pins: selected.map((pin) => `${pin.name}@${pin.version}`),
  };
}

function usage() {
  return [
    'usage: node self-hosting/sdk-compat/resolve-pinned-sdk.mjs [--print scope|version|json] [--examples-dir DIR]',
    '',
    'Prints the exact SDK scope and version pinned by examples/*/package.json.',
    'The BackendRelay scope wins when an example already uses it.',
  ].join('\n');
}

function main(argv) {
  let field = 'json';
  let examplesRoot = join(repoRoot, 'examples');
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    switch (argument) {
      case '--print':
        field = argv[index + 1];
        index += 1;
        break;
      case '--examples-dir':
        examplesRoot = resolve(argv[index + 1]);
        index += 1;
        break;
      case '-h':
      case '--help':
        console.log(usage());
        return 0;
      default:
        console.error(`unknown argument: ${argument}\n\n${usage()}`);
        return 2;
    }
  }

  if (!['scope', 'version', 'json'].includes(field)) {
    console.error(`--print must be scope, version or json\n\n${usage()}`);
    return 2;
  }

  const resolved = resolvePinnedSdk(examplesRoot);
  if (field === 'json') {
    console.log(JSON.stringify(resolved));
  } else {
    console.log(resolved[field]);
  }
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = main(process.argv.slice(2));
}
