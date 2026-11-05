import { afterEach, expect, test, vi } from 'vitest';
import { init } from './index.js';

afterEach(() => vi.unstubAllGlobals());

const database = () =>
  init({
    appId: 'app',
    adminToken: 'fixture',
    apiURI: 'https://api.example.test',
  });

test('removed execution management APIs are absent from the admin client', () => {
  const db = database();
  for (const key of [
    'jobs',
    'tasks',
    'workflows',
    'crons',
    'runs',
    'queues',
    'deployments',
    'secrets',
    'limits',
    'audit',
    'metrics',
  ]) {
    expect(key in db).toBe(false);
  }
  expect(db.query).toBeTypeOf('function');
  expect(db.storage.uploadFile).toBeTypeOf('function');
});

test('legacy JavaScript transaction options reject before sending a partial mutation', async () => {
  const fetch = vi.fn();
  vi.stubGlobal('fetch', fetch);
  const db = database();
  const options = { enqueue: [{ task: 'notify', input: {} }] };
  // @ts-expect-error The former execution options are intentionally no longer part of the API.
  const result = db.transact([], options);
  await expect(result).rejects.toThrow(
    'Transaction options are no longer supported',
  );
  expect(fetch).not.toHaveBeenCalled();
});

test('ordinary transactions retain the upstream request and response shape', async () => {
  const fetch = vi.fn(
    async (_input: unknown, _init: RequestInit) =>
      new Response(JSON.stringify({ 'tx-id': 42 }), { status: 200 }),
  );
  vi.stubGlobal('fetch', fetch);
  await expect(database().transact([])).resolves.toEqual({ 'tx-id': 42 });
  expect(JSON.parse(fetch.mock.calls[0][1].body as string)).toEqual({
    steps: [],
    'throw-on-missing-attrs?': false,
  });
});
