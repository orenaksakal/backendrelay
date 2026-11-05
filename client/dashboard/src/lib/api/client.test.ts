import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { z } from 'zod';
import { loadRuntimeConfig, resetRuntimeConfig } from '@/lib/runtime-config';
import { server } from '@/test/msw/node';
import {
  API_SOURCE,
  API_SOURCE_HEADER,
  ApiError,
  apiBaseUrlFor,
  apiFetch,
  buildApiUrl,
  setApiToken,
  setUnauthorizedHandler,
} from './client';
import { defineEndpoint } from './endpoints/types';

const pingEndpoint = defineEndpoint({
  method: 'GET',
  path: '/dash/apps/:appId/ping',
  response: z.looseObject({ ok: z.boolean(), extra: z.string().optional() }),
});

const baseConfig = { apiBaseUrl: 'http://localhost:8888' };

beforeEach(async () => {
  resetRuntimeConfig();
  setApiToken(null);
  setUnauthorizedHandler(null);
  await loadRuntimeConfig();
});

afterEach(() => {
  resetRuntimeConfig();
  setApiToken(null);
  setUnauthorizedHandler(null);
});

describe('URL routing', () => {
  it('keeps management paths on the API origin by default', () => {
    expect(apiBaseUrlFor('/dash', baseConfig)).toBe('http://localhost:8888');
    expect(apiBaseUrlFor('/dash/apps/app-1', baseConfig)).toBe(
      'http://localhost:8888',
    );
    expect(apiBaseUrlFor('/superadmin/apps', baseConfig)).toBe(
      'http://localhost:8888',
    );
  });

  it('sends /dash, /superadmin and /platform/oauth to the gateway when set', () => {
    const gateway = {
      apiBaseUrl: 'http://localhost:8888',
      dashboardApiUrl: 'https://gateway.example.com/gateway',
    };

    expect(apiBaseUrlFor('/dash', gateway)).toBe(
      'https://gateway.example.com/gateway',
    );
    expect(apiBaseUrlFor('/dash/apps/app-1', gateway)).toBe(
      'https://gateway.example.com/gateway',
    );
    expect(apiBaseUrlFor('/superadmin/apps', gateway)).toBe(
      'https://gateway.example.com/gateway',
    );
    expect(apiBaseUrlFor('/platform/oauth/claim', gateway)).toBe(
      'https://gateway.example.com/gateway',
    );
  });

  it('leaves runtime and lookalike paths on the API origin', () => {
    const gateway = {
      apiBaseUrl: 'http://localhost:8888',
      dashboardApiUrl: 'https://gateway.example.com',
    };

    expect(apiBaseUrlFor('/admin/query', gateway)).toBe(
      'http://localhost:8888',
    );
    expect(apiBaseUrlFor('/webhooks/payload/a/b/c', gateway)).toBe(
      'http://localhost:8888',
    );
    expect(apiBaseUrlFor('/dashboards', gateway)).toBe('http://localhost:8888');
  });

  it('fills path parameters and appends the rest as query parameters', () => {
    const url = buildApiUrl(
      '/dash/apps/:appId/ping',
      { appId: 'app-1', view: 'grid', page: 2, flag: true, skip: undefined },
      baseConfig,
    );

    expect(url).toBe(
      'http://localhost:8888/dash/apps/app-1/ping?view=grid&page=2&flag=true',
    );
  });

  it('keeps a gateway path prefix', () => {
    const url = buildApiUrl('/dash/apps', undefined, {
      apiBaseUrl: 'http://localhost:8888',
      dashboardApiUrl: 'https://gateway.example.com/gateway',
    });

    expect(url).toBe('https://gateway.example.com/gateway/dash/apps');
  });

  it('escapes path parameters and rejects missing ones', () => {
    expect(
      buildApiUrl('/dash/apps/:appId', { appId: 'a/b c' }, baseConfig),
    ).toBe('http://localhost:8888/dash/apps/a%2Fb%20c');

    expect(() => buildApiUrl('/dash/apps/:appId', {}, baseConfig)).toThrow(
      ApiError,
    );
  });

  it('keeps slashes in an ISN while escaping each segment', () => {
    const path = '/dash/apps/:appId/webhooks/:webhookId/events/:isn';
    expect(
      buildApiUrl(
        path,
        { appId: 'app-1', webhookId: 'webhook-1', isn: '0/328/48953748' },
        baseConfig,
        ['isn'],
      ),
    ).toBe(
      'http://localhost:8888/dash/apps/app-1/webhooks/webhook-1/events/0/328/48953748',
    );

    expect(
      buildApiUrl(
        path,
        { appId: 'app-1', webhookId: 'w', isn: '0/3 2' },
        baseConfig,
        ['isn'],
      ),
    ).toBe('http://localhost:8888/dash/apps/app-1/webhooks/w/events/0/3%202');

    expect(() =>
      buildApiUrl(
        path,
        { appId: 'app-1', webhookId: 'w', isn: '../../tokens/rotate' },
        baseConfig,
        ['isn'],
      ),
    ).toThrow(ApiError);
  });
});

describe('apiFetch', () => {
  it('sends the bearer token and the dashboard source header', async () => {
    setApiToken('session-token');
    const seen: { headers?: Headers } = {};
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', ({ request }) => {
        seen.headers = request.headers;
        return HttpResponse.json({ ok: true, extra: 'ignored' });
      }),
    );

    const result = await apiFetch(pingEndpoint, {
      params: { appId: 'app-1' },
    });

    expect(result).toEqual({ ok: true, extra: 'ignored' });
    const headers = seen.headers;
    if (headers === undefined) throw new Error('the request was not sent');
    expect(headers.get('authorization')).toBe('Bearer session-token');
    expect(headers.get(API_SOURCE_HEADER.toLowerCase())).toBe(API_SOURCE);
  });

  it('routes /dash through the gateway origin when configured', async () => {
    resetRuntimeConfig();
    server.use(
      http.get('/config.json', () =>
        HttpResponse.json({
          apiBaseUrl: 'http://localhost:8888',
          dashboardApiUrl: 'https://gateway.example.com',
        }),
      ),
    );
    await loadRuntimeConfig();

    let seenUrl = '';
    server.use(
      http.get(
        'https://gateway.example.com/dash/apps/:appId/ping',
        ({ request }) => {
          seenUrl = request.url;
          return HttpResponse.json({ ok: true });
        },
      ),
    );

    await apiFetch(pingEndpoint, { params: { appId: 'app-1' } });

    expect(seenUrl).toBe('https://gateway.example.com/dash/apps/app-1/ping');
  });

  it('parses server error bodies, preferring the body trace ID', async () => {
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          {
            type: 'permission-denied',
            message: 'You do not have access.',
            hint: { 'record-type': 'app' },
            'trace-id': 'trace-body',
          },
          { status: 403, headers: { 'x-request-id': 'trace-header' } },
        ),
      ),
    );

    const error = await apiFetch(pingEndpoint, {
      params: { appId: 'app-1' },
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    if (!(error instanceof ApiError)) throw error;
    expect(error.status).toBe(403);
    expect(error.type).toBe('permission-denied');
    expect(error.message).toBe('You do not have access.');
    expect(error.hint).toEqual({ 'record-type': 'app' });
    expect(error.requestId).toBe('trace-body');
  });

  it('falls back to the x-request-id header', async () => {
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          {},
          { status: 500, headers: { 'x-request-id': 'req-header' } },
        ),
      ),
    );

    const error = await apiFetch(pingEndpoint, {
      params: { appId: 'app-1' },
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    if (!(error instanceof ApiError)) throw error;
    expect(error.status).toBe(500);
    expect(error.type).toBe('http-error');
    expect(error.message).toBe(
      'The server is unavailable (HTTP 500). Try again.',
    );
    expect(error.requestId).toBe('req-header');
  });

  it('maps oauth-error bodies to their error message', async () => {
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          { type: 'oauth-error', error: 'invalid_grant' },
          { status: 400 },
        ),
      ),
    );

    const error = await apiFetch(pingEndpoint, {
      params: { appId: 'app-1' },
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    if (!(error instanceof ApiError)) throw error;
    expect(error.type).toBe('oauth-error');
    expect(error.message).toBe('invalid_grant');
  });

  it('rejects responses that do not match the endpoint schema', async () => {
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          { ok: 'yes' },
          { headers: { 'x-request-id': 'req-shape' } },
        ),
      ),
    );

    const error = await apiFetch(pingEndpoint, {
      params: { appId: 'app-1' },
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    if (!(error instanceof ApiError)) throw error;
    expect(error.status).toBe(200);
    expect(error.type).toBe('invalid-response');
    expect(error.requestId).toBe('req-shape');
    expect(error.hint).toBeDefined();
  });
});

describe('401 handling', () => {
  it('tells the session layer before the error reaches the caller', async () => {
    const unauthorized = vi.fn();
    setUnauthorizedHandler(unauthorized);
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          { type: 'record-not-found', message: 'Your session expired.' },
          { status: 401 },
        ),
      ),
    );

    const error = await apiFetch(pingEndpoint, {
      params: { appId: 'app-1' },
    }).catch((caught: unknown) => caught);

    expect(unauthorized).toHaveBeenCalledTimes(1);
    expect(error).toBeInstanceOf(ApiError);
    if (!(error instanceof ApiError)) throw error;
    expect(error.status).toBe(401);
  });

  it('leaves other failures to the caller', async () => {
    const unauthorized = vi.fn();
    setUnauthorizedHandler(unauthorized);
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          { type: 'permission-denied', message: 'You do not have access.' },
          { status: 403 },
        ),
      ),
    );

    await apiFetch(pingEndpoint, { params: { appId: 'app-1' } }).catch(
      () => undefined,
    );

    expect(unauthorized).not.toHaveBeenCalled();
  });
});
