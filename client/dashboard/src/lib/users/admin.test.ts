import { HttpResponse, http } from 'msw';
import { describe, expect, it } from 'vitest';
import { ApiError } from '@/lib/api/client';
import { server } from '@/test/msw/node';
import {
  appAdminConfig,
  appAdminUrl,
  createUser,
  deleteUser,
  sendMagicCode,
  signOutUser,
} from './admin';

const API = 'http://localhost:8888';

const config = {
  apiBaseUrl: API,
  appId: 'app-1',
  adminToken: 'admin-token',
};

interface Seen {
  url: URL;
  authorization: string | null;
  appId: string | null;
  body: unknown;
}

async function capture(request: Request): Promise<Seen> {
  const text = await request.text();
  return {
    url: new URL(request.url),
    authorization: request.headers.get('authorization'),
    appId: request.headers.get('app-id'),
    body: text === '' ? undefined : (JSON.parse(text) as unknown),
  };
}

describe('admin config', () => {
  it('needs an API origin and an admin token', () => {
    expect(
      appAdminConfig({
        appId: 'app-1',
        adminToken: 'admin-token',
        apiBaseUrl: API,
      }),
    ).toEqual(config);
    expect(
      appAdminConfig({ appId: 'app-1', adminToken: '', apiBaseUrl: API }),
    ).toBeNull();
    expect(
      appAdminConfig({
        appId: 'app-1',
        adminToken: 'token',
        apiBaseUrl: undefined,
      }),
    ).toBeNull();
  });

  it('keeps app_id and selector params on the app origin', () => {
    expect(appAdminUrl(config, '/admin/users', { id: 'user-1' })).toBe(
      'http://localhost:8888/admin/users?app_id=app-1&id=user-1',
    );
    expect(
      appAdminUrl({ ...config, apiBaseUrl: `${API}/` }, '/admin/users'),
    ).toBe('http://localhost:8888/admin/users?app_id=app-1');
  });
});

describe('admin auth calls', () => {
  it('sends a magic code with the app admin token and never returns it', async () => {
    const seen: Seen[] = [];
    server.use(
      http.post(`${API}/admin/send_magic_code`, async ({ request }) => {
        seen.push(await capture(request));
        return HttpResponse.json({ code: '424242' });
      }),
    );

    await expect(
      sendMagicCode(config, 'ada@example.com'),
    ).resolves.toBeUndefined();

    const call = seen[0];
    expect(call?.url.pathname).toBe('/admin/send_magic_code');
    expect(call?.url.searchParams.get('app_id')).toBe('app-1');
    expect(call?.authorization).toBe('Bearer admin-token');
    expect(call?.appId).toBe('app-1');
    expect(call?.body).toEqual({ email: 'ada@example.com' });
  });

  it('creates a user without exposing the refresh token', async () => {
    const seen: Seen[] = [];
    server.use(
      http.post(`${API}/admin/refresh_tokens`, async ({ request }) => {
        seen.push(await capture(request));
        return HttpResponse.json({
          user: { id: 'user-1', refresh_token: 'secret-token' },
        });
      }),
    );

    await expect(
      createUser(config, 'new@example.com'),
    ).resolves.toBeUndefined();
    expect(seen[0]?.body).toEqual({ email: 'new@example.com' });
  });

  it('signs a user out by id', async () => {
    const seen: Seen[] = [];
    server.use(
      http.post(`${API}/admin/sign_out`, async ({ request }) => {
        seen.push(await capture(request));
        return HttpResponse.json({ ok: true });
      }),
    );

    await signOutUser(config, 'user-1');

    expect(seen[0]?.body).toEqual({ id: 'user-1' });
  });

  it('deletes a user through the query selector', async () => {
    const seen: Seen[] = [];
    server.use(
      http.delete(`${API}/admin/users`, async ({ request }) => {
        seen.push(await capture(request));
        return HttpResponse.json({ deleted: { id: 'user-1' } });
      }),
    );

    await deleteUser(config, 'user-1');

    expect(seen[0]?.url.searchParams.get('app_id')).toBe('app-1');
    expect(seen[0]?.url.searchParams.get('id')).toBe('user-1');
  });

  it('maps server failures onto ApiError', async () => {
    server.use(
      http.post(`${API}/admin/send_magic_code`, () =>
        HttpResponse.json(
          { type: 'record-not-found', message: 'No user found' },
          { status: 404 },
        ),
      ),
    );

    await expect(
      sendMagicCode(config, 'missing@example.com'),
    ).rejects.toMatchObject({ name: 'ApiError', status: 404 });
    await expect(
      sendMagicCode(config, 'missing@example.com'),
    ).rejects.toBeInstanceOf(ApiError);
  });
});
