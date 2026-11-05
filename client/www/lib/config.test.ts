import { expect, test, vi } from 'vitest';
import { configFromApiURI, requireSelfHostedConfig } from './config';

test('unconfigured dashboard never defaults to upstream production', async () => {
  vi.resetModules();
  vi.stubEnv('NEXT_PUBLIC_BACKEND_URL', '');
  vi.stubEnv('NEXT_PUBLIC_SELF_HOSTED', 'false');
  vi.stubEnv('NEXT_PUBLIC_LOCAL_SERVER_PORT', '8888');
  try {
    const { config } = await import('./config');
    expect(config.apiURI).toBe('http://localhost:8888');
  } finally {
    vi.unstubAllEnvs();
  }
});

test('explicit backend URL supplies both API and WebSocket origins', async () => {
  vi.resetModules();
  vi.stubEnv('NEXT_PUBLIC_BACKEND_URL', 'https://rust.example.com');
  vi.stubEnv('NEXT_PUBLIC_SELF_HOSTED', 'false');
  try {
    const { config } = await import('./config');
    expect(config).toEqual({
      apiURI: 'https://rust.example.com',
      websocketURI: 'wss://rust.example.com/runtime/session',
    });
  } finally {
    vi.unstubAllEnvs();
  }
});

test('builds dashboard config from an API URI', () => {
  expect(configFromApiURI('https://api.example.com')).toEqual({
    apiURI: 'https://api.example.com',
    websocketURI: 'wss://api.example.com/runtime/session',
  });
  expect(configFromApiURI('http://api.example.com')).toEqual({
    apiURI: 'http://api.example.com',
    websocketURI: 'ws://api.example.com/runtime/session',
  });
});

test('requires valid self-hosted dashboard config', () => {
  expect(() => requireSelfHostedConfig(undefined)).toThrow(
    'Self-hosted dashboard requires INSTANT_API_URI or INSTANT_BACKEND_URL to be a valid HTTP(S) URL.',
  );
  expect(() => requireSelfHostedConfig('not a url')).toThrow(
    'Self-hosted dashboard requires INSTANT_API_URI or INSTANT_BACKEND_URL to be a valid HTTP(S) URL.',
  );
  expect(() => requireSelfHostedConfig('ftp://api.example.com')).toThrow(
    'Self-hosted dashboard requires INSTANT_API_URI or INSTANT_BACKEND_URL to be a valid HTTP(S) URL.',
  );
});

test('private management URL does not change public SDK or realtime configuration', () => {
  const config = requireSelfHostedConfig(
    'https://api.example.com',
    'https://private.test.ts.net/__instant_api/',
  );
  expect(config).toEqual({
    apiURI: 'https://api.example.com',
    websocketURI: 'wss://api.example.com/runtime/session',
    dashboardApiURI: 'https://private.test.ts.net/__instant_api',
  });
  for (const invalid of [
    'not a URL',
    'ftp://private.test',
    'https://user:password@private.test',
    'https://private.test?token=secret',
  ]) {
    expect(() =>
      requireSelfHostedConfig('https://api.example.com', invalid),
    ).toThrow();
  }
});
