#!/usr/bin/env node

// Reads the oldest SDK version still reported in production.
//
// P4.5 records backendrelay_sdk_sessions{sdk,version} for live sessions. The
// compatibility lane runs the Core and Admin e2e suites against this version.
// The CI runner deliberately carries no production credentials, so an operator
// runs this script where Prometheus is reachable and stores the result in the
// SDK_COMPAT_OLDEST_VERSION repository variable.

import { pathToFileURL } from 'node:url';

export const METRIC = 'backendrelay_sdk_sessions';

function parseVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/.exec(version);
  if (!match) return undefined;
  return {
    parts: [Number(match[1]), Number(match[2]), Number(match[3])],
    prerelease: match[4],
  };
}

function compareVersions(left, right) {
  for (let index = 0; index < left.parts.length; index += 1) {
    if (left.parts[index] !== right.parts[index]) {
      return left.parts[index] - right.parts[index];
    }
  }
  // A release sorts after its prereleases; the exact prerelease order is not
  // needed because only the oldest reported release matters here.
  if (left.prerelease === right.prerelease) return 0;
  if (left.prerelease === undefined) return 1;
  if (right.prerelease === undefined) return -1;
  return left.prerelease.localeCompare(right.prerelease);
}

// `series` is the Prometheus API `data.result` array. Versions that cannot be
// parsed and the bounded `other` label are ignored.
export function oldestVersion(series) {
  const versions = [];
  for (const sample of series ?? []) {
    const version = sample?.metric?.version;
    const value = Number(sample?.value?.[1]);
    if (!version || version === 'other' || !Number.isFinite(value) || value <= 0) {
      continue;
    }
    const parsed = parseVersion(version);
    if (parsed) versions.push({ version, parsed });
  }
  versions.sort((left, right) => compareVersions(left.parsed, right.parsed));
  return versions[0]?.version;
}

export async function queryOldestVersion({
  url,
  token,
  query = METRIC,
  timeoutMs = 10_000,
  fetchImpl = fetch,
}) {
  if (!url) throw new Error('PROMETHEUS_URL is not set');
  const endpoint = new URL('/api/v1/query', url);
  endpoint.searchParams.set('query', query);
  const headers = { accept: 'application/json' };
  if (token) headers.authorization = `Bearer ${token}`;
  const response = await fetchImpl(endpoint, {
    headers,
    signal: AbortSignal.timeout(timeoutMs),
  });
  if (!response.ok) {
    throw new Error(
      `Prometheus query failed: ${response.status} ${await response.text()}`,
    );
  }
  const payload = await response.json();
  if (payload.status !== 'success') {
    throw new Error(`Prometheus query failed: ${payload.error ?? payload.status}`);
  }
  return oldestVersion(payload.data?.result);
}

function usage() {
  return [
    'usage: PROMETHEUS_URL=https://... [PROMETHEUS_TOKEN=...] node self-hosting/sdk-compat/oldest-sdk-version.mjs [--allow-empty] [--query PROMQL]',
    '',
    `Queries ${METRIC} and prints the oldest semver still reported.`,
    'Store the result in the SDK_COMPAT_OLDEST_VERSION repository variable.',
  ].join('\n');
}

function parseArgs(argv) {
  const options = {
    allowEmpty: false,
    query: METRIC,
    url: process.env.PROMETHEUS_URL,
    token: process.env.PROMETHEUS_TOKEN,
  };
  for (let index = 0; index < argv.length; index += 1) {
    switch (argv[index]) {
      case '--allow-empty':
        options.allowEmpty = true;
        break;
      case '--query':
        options.query = argv[index + 1];
        index += 1;
        break;
      case '--url':
        options.url = argv[index + 1];
        index += 1;
        break;
      case '-h':
      case '--help':
        return { help: true };
      default:
        throw new Error(`unknown argument: ${argv[index]}\n\n${usage()}`);
    }
  }
  return options;
}

async function main(argv) {
  let options;
  try {
    options = parseArgs(argv);
  } catch (error) {
    console.error(`${error.message}\n\n${usage()}`);
    return 2;
  }
  if (options.help) {
    console.log(usage());
    return 0;
  }
  if (!options.url) {
    console.error(`PROMETHEUS_URL is not set\n\n${usage()}`);
    return 2;
  }
  const oldest = await queryOldestVersion(options);
  if (!oldest) {
    if (options.allowEmpty) return 0;
    console.error(`no version labels reported by ${METRIC}`);
    return 1;
  }
  console.log(oldest);
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).then(
    (status) => {
      process.exitCode = status;
    },
    (error) => {
      console.error(error.message);
      process.exitCode = 1;
    },
  );
}
