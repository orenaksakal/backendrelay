import { QueryClient } from '@tanstack/react-query';
import { isNotFound } from '@tanstack/react-router';
import { HttpResponse, http } from 'msw';
import { beforeEach, describe, expect, it } from 'vitest';
import { loadRuntimeConfig, resetRuntimeConfig } from '@/lib/runtime-config';
import { server } from '@/test/msw/node';
import { loadAppRouteContext } from './app-loader';

const API = 'http://localhost:8888';

function newQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
}

const app = {
  id: 'app-1',
  title: 'todo-app',
  created_at: '2026-01-01T00:00:00Z',
  role: 'admin',
  org: { id: 'org-1', title: 'Acme' },
  org_id: 'org-1',
};

beforeEach(async () => {
  resetRuntimeConfig();
  await loadRuntimeConfig();
});

describe('loadAppRouteContext', () => {
  it('reads an app from the bootstrap payload without another request', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          apps: [app],
          orgs: [],
          invites: [],
          superuser: false,
          user: { id: 'user-1', email: 'owner@example.com' },
        }),
      ),
    );

    const context = await loadAppRouteContext(newQueryClient(), 'app-1');

    expect(context.app.title).toBe('todo-app');
    expect(context.role).toBe('admin');
    expect(context.workspaceId).toBe('org-1');
  });

  it('fetches an app that is not in the bootstrap payload', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          apps: [],
          orgs: [],
          invites: [],
          superuser: false,
          user: { id: 'user-1', email: 'owner@example.com' },
        }),
      ),
      http.get(`${API}/dash/apps/:appId`, ({ params }) =>
        HttpResponse.json({
          app: { ...app, id: params.appId, org: null, org_id: null },
        }),
      ),
    );

    const context = await loadAppRouteContext(newQueryClient(), 'app-1');

    expect(context.role).toBe('admin');
    expect(context.workspaceId).toBe('personal');
  });

  it('turns a missing app into the route not-found state', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          apps: [],
          orgs: [],
          invites: [],
          superuser: false,
          user: { id: 'user-1', email: 'owner@example.com' },
        }),
      ),
      http.get(`${API}/dash/apps/:appId`, () =>
        HttpResponse.json({ message: 'App not found.' }, { status: 404 }),
      ),
    );

    const error = await loadAppRouteContext(newQueryClient(), 'missing').catch(
      (caught: unknown) => caught,
    );

    expect(isNotFound(error)).toBe(true);
  });
});
