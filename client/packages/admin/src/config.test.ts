import { afterEach, describe, expect, test, vi } from 'vitest';

import { init } from './index.js';
import { InstantAPIError } from '@instantdb/core';

const appId = '00000000-0000-4000-8000-000000000000';
const apiURI = 'https://api.example.test';

afterEach(() => vi.unstubAllGlobals());

describe('admin backend URL configuration', () => {
  test('init requires an apiURI instead of defaulting to instantdb.com', () => {
    expect(() => init({ appId, adminToken: 'token' })).toThrow(/apiURI/);
  });

  test('uses the configured API origin for requests', async () => {
    const fetch = vi.fn(
      async (_input: RequestInfo, _init?: RequestInit) =>
        new Response(JSON.stringify({ data: { goals: [] } }), {
          status: 200,
        }),
    );
    vi.stubGlobal('fetch', fetch);

    const db = init({
      appId,
      adminToken: 'token',
      apiURI: `${apiURI}/`,
    });
    await db.query({ goals: {} });

    expect(fetch).toHaveBeenCalledOnce();
    expect(String(fetch.mock.calls[0][0])).toBe(
      `${apiURI}/admin/query?app_id=${appId}`,
    );
  });
});

describe('admin onError hook', () => {
  test('receives request failures', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('{"message":"nope"}', { status: 500 })),
    );
    const errors: Error[] = [];
    const db = init({
      appId,
      adminToken: 'token',
      apiURI,
      onError: (error) => errors.push(error),
    });

    await expect(db.query({ goals: {} })).rejects.toBeInstanceOf(
      InstantAPIError,
    );
    expect(errors).toHaveLength(1);
    expect(errors[0]).toBeInstanceOf(InstantAPIError);
  });
});
