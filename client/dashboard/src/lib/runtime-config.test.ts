import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { server } from '@/test/msw/node';
import {
  loadRuntimeConfig,
  parseRuntimeConfig,
  resetRuntimeConfig,
} from './runtime-config';

beforeEach(() => {
  resetRuntimeConfig();
});

afterEach(() => {
  resetRuntimeConfig();
});

describe('parseRuntimeConfig', () => {
  it('accepts the API URL and treats blank optionals as unset', () => {
    const config = parseRuntimeConfig({
      apiBaseUrl: 'https://api.example.com',
      dashboardApiUrl: '',
      storageOrigin: 'https://storage.example.com',
    });

    expect(config.apiBaseUrl).toBe('https://api.example.com');
    expect(config.dashboardApiUrl).toBeUndefined();
    expect(config.storageOrigin).toBe('https://storage.example.com');
  });

  it('accepts gateway mode and a storage origin', () => {
    const config = parseRuntimeConfig({
      apiBaseUrl: 'https://api.example.com',
      dashboardApiUrl: 'https://gateway.example.com',
      storageOrigin: 'https://storage.example.com',
    });

    expect(config.dashboardApiUrl).toBe('https://gateway.example.com');
  });

  it('rejects a missing or malformed API URL', () => {
    expect(() => parseRuntimeConfig({})).toThrow(/apiBaseUrl/);
    expect(() => parseRuntimeConfig({ apiBaseUrl: 'localhost:8888' })).toThrow(
      /apiBaseUrl/,
    );
  });
});

describe('loadRuntimeConfig', () => {
  it('loads and validates /config.json once', async () => {
    const first = await loadRuntimeConfig();

    expect(first).toEqual({
      ok: true,
      config: { apiBaseUrl: 'http://localhost:8888' },
    });
    await expect(loadRuntimeConfig()).resolves.toBe(first);
  });

  it('returns an error result when /config.json is invalid', async () => {
    server.use(
      http.get('/config.json', () =>
        HttpResponse.json({ dashboardApiUrl: 'http://gateway.local' }),
      ),
    );

    const result = await loadRuntimeConfig();

    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.message).toMatch(/apiBaseUrl/);
  });

  it('returns an error result when /config.json is unreachable', async () => {
    server.use(
      http.get('/config.json', () => new HttpResponse(null, { status: 404 })),
    );

    const result = await loadRuntimeConfig();

    expect(result).toEqual({
      ok: false,
      message: 'Could not load /config.json: HTTP 404',
    });
  });
});
