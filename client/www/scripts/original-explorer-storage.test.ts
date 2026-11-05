import assert from 'node:assert/strict';
import test from 'node:test';
import { explorerStorageHandlers } from '../components/dash/explorer/storage';

test('the original Explorer uploads Unicode paths using the dashboard session, not the app admin token', async () => {
  const calls: { url: string; init: RequestInit }[] = [];
  const storage = explorerStorageHandlers(
    'https://api.example.test',
    'app',
    'session',
    async (url, init) => {
      calls.push({ url, init });
    },
  );
  const file = new File(['sample'], 'photo.png', { type: 'image/png' });
  await storage.onUploadFile(file, 'photos/日本語.png');
  assert.equal(
    calls[0].url,
    'https://api.example.test/dash/apps/app/storage/upload?path=photos%2F%E6%97%A5%E6%9C%AC%E8%AA%9E.png',
  );
  const headers = new Headers(calls[0].init.headers);
  assert.equal(headers.get('authorization'), 'Bearer session');
  assert.equal(headers.get('path'), null);
  assert.equal(calls[0].init.method, 'PUT');
  assert.equal(calls[0].init.body, file);
});

test('file deletion resolves only the selected loaded paths and uses the authenticated storage endpoint', async () => {
  let call: { url: string; init: RequestInit } | undefined;
  const storage = explorerStorageHandlers(
    'https://api.example.test',
    'app',
    'session',
    async (url, init) => {
      call = { url, init };
    },
  );
  await storage.onDeleteFiles(
    [
      { id: 'first', path: 'keep.png' },
      { id: 'second', path: '日本語.png' },
    ],
    ['second'],
  );
  assert.equal(
    call?.url,
    'https://api.example.test/dash/apps/app/storage/files/delete',
  );
  assert.equal(call?.init.method, 'POST');
  assert.deepEqual(JSON.parse(call?.init.body as string), {
    filenames: ['日本語.png'],
  });
  assert.equal(
    new Headers(call?.init.headers).get('authorization'),
    'Bearer session',
  );
});

test('failed and stale file operations reject without retrying mutations', async () => {
  let calls = 0;
  const storage = explorerStorageHandlers(
    'https://api.example.test',
    'app',
    'session',
    async () => {
      calls++;
      throw new Error('denied');
    },
  );
  await assert.rejects(
    () => storage.onDeleteFiles([], ['missing']),
    /no longer loaded/,
  );
  assert.equal(calls, 0);
  await assert.rejects(
    () => storage.onDeleteFiles([{ id: 'file', path: 'a.png' }], ['file']),
    /denied/,
  );
  assert.equal(calls, 1);
});
