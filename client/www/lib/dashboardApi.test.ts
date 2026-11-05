import { expect, test } from 'vitest';
import { routeDashboardURL } from './dashboardApi';

const api = 'https://api.example.com';
const management = 'https://private.test.ts.net/__instant_api';

test.each([
  '/dash',
  '/dash/me',
  '/dash/restores/zip',
  '/superadmin/apps',
  '/platform/oauth/token',
])('management route %s uses the private gateway', (path) => {
  expect(routeDashboardURL(api + path + '?a=1', api, management)).toBe(
    management + path + '?a=1',
  );
});

test.each([
  '/admin/query',
  '/runtime/session',
  '/runtime/auth/send_magic_code',
  '/storage/upload',
  '/webhooks/payload/a/b/c',
  '/.well-known/webhooks/jwks.json',
  '/health/system',
  '/dashboard',
  '/platform/oauth-other',
])('public route %s is never sent to the private gateway', (path) => {
  expect(routeDashboardURL(api + path, api, management)).toBe(api + path);
});

test('external URLs, signed objects, and already-private requests are unchanged', () => {
  for (const url of [
    'https://s3.example.com/dash/backup?signature=opaque',
    management + '/dash/me',
    '/public.js',
  ]) {
    expect(routeDashboardURL(url, api, management)).toBe(url);
  }
  expect(routeDashboardURL(api + '/dash/me', api, api)).toBe(api + '/dash/me');
});

test('management WebSockets and non-root backend URLs retain their path and query', () => {
  expect(
    routeDashboardURL(
      'wss://api.example.com/dash/session_counts?app_id=a',
      api,
      management,
    ),
  ).toBe(
    'wss://private.test.ts.net/__instant_api/dash/session_counts?app_id=a',
  );
  expect(
    routeDashboardURL(api + '/prefix/dash/me', api + '/prefix', management),
  ).toBe(management + '/dash/me');
});
