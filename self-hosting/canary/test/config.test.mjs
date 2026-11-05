import assert from 'node:assert/strict';
import test from 'node:test';

import {
  ConfigError,
  DEFAULT_INTERVAL_SECONDS,
  DEFAULT_METRICS_ADDR,
  DEFAULT_TIMEOUT_SECONDS,
  parseConfig,
  readAdminToken,
} from '../src/config.mjs';

const APP_ID = '22222222-2222-4222-8222-222222222222';
const TOKEN = '11111111-1111-4111-8111-111111111111';
const BASE = {
  CANARY_API_URL: 'https://api.example.com/',
  CANARY_APP_ID: APP_ID,
  CANARY_ADMIN_TOKEN_FILE: '/run/secrets/canary-admin-token',
};

test('parses required settings and applies defaults', () => {
  const config = parseConfig(BASE);
  assert.equal(config.apiUrl, 'https://api.example.com');
  assert.equal(config.appId, APP_ID);
  assert.equal(config.adminTokenFile, '/run/secrets/canary-admin-token');
  assert.equal(config.healthcheckUrl, null);
  assert.equal(config.intervalSeconds, DEFAULT_INTERVAL_SECONDS);
  assert.equal(config.intervalMs, DEFAULT_INTERVAL_SECONDS * 1000);
  assert.equal(config.timeoutSeconds, DEFAULT_TIMEOUT_SECONDS);
  assert.equal(config.timeoutMs, DEFAULT_TIMEOUT_SECONDS * 1000);
  const [host, port] = DEFAULT_METRICS_ADDR.split(':');
  assert.equal(config.metricsHost, host);
  assert.equal(config.metricsPort, Number(port));
  assert.ok(Object.isFrozen(config));
});

test('parses overrides and optional healthcheck URL', () => {
  const config = parseConfig({
    ...BASE,
    CANARY_API_URL: 'http://127.0.0.1:8888',
    CANARY_INTERVAL_SECONDS: '15',
    CANARY_TIMEOUT_SECONDS: '3',
    CANARY_METRICS_ADDR: '127.0.0.1:9999',
    CANARY_HEALTHCHECK_URL: 'https://hc-ping.com/example',
  });
  assert.equal(config.apiUrl, 'http://127.0.0.1:8888');
  assert.equal(config.intervalSeconds, 15);
  assert.equal(config.timeoutSeconds, 3);
  assert.equal(config.metricsHost, '127.0.0.1');
  assert.equal(config.metricsPort, 9999);
  assert.equal(config.healthcheckUrl, 'https://hc-ping.com/example');
});

test('rejects missing or unsafe configuration', () => {
  const invalid = [
    {},
    { ...BASE, CANARY_API_URL: 'ftp://api.example.com' },
    { ...BASE, CANARY_API_URL: 'not a url' },
    { ...BASE, CANARY_API_URL: 'https://user:pass@api.example.com' },
    { ...BASE, CANARY_API_URL: 'https://api.example.com/?token=secret' },
    { ...BASE, CANARY_APP_ID: 'not-a-uuid' },
    { ...BASE, CANARY_ADMIN_TOKEN_FILE: ' ' },
    { ...BASE, CANARY_HEALTHCHECK_URL: 'ftp://hc-ping.com' },
    { ...BASE, CANARY_INTERVAL_SECONDS: '0' },
    { ...BASE, CANARY_INTERVAL_SECONDS: '4' },
    { ...BASE, CANARY_INTERVAL_SECONDS: '1.5' },
    { ...BASE, CANARY_TIMEOUT_SECONDS: '61' },
    { ...BASE, CANARY_METRICS_ADDR: ':9465' },
    { ...BASE, CANARY_METRICS_ADDR: '0.0.0.0:99999' },
  ];
  for (const env of invalid) {
    assert.throws(() => parseConfig(env), ConfigError, JSON.stringify(env));
  }
});

test('reads the admin token from a secret file only', async () => {
  const token = await readAdminToken('/run/secrets/canary', async () => `${TOKEN}\n`);
  assert.equal(token, TOKEN);
  await assert.rejects(
    readAdminToken('/run/secrets/canary', async () => '   \n'),
    ConfigError,
  );
  await assert.rejects(
    readAdminToken('/run/secrets/canary', async () => 'not-a-token'),
    ConfigError,
  );
  await assert.rejects(
    readAdminToken('/run/secrets/canary', async () => {
      throw new Error('ENOENT');
    }),
    ConfigError,
  );
});
