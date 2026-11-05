import { afterEach, expect, test, vi } from 'vitest';
import IndexedDBStorage from '../../src/IndexedDBStorage';
import { createDevtool } from '../../src/devtool';

afterEach(() => vi.unstubAllGlobals());

test('native browser persistence survives reopening and independent mutation deletion', async () => {
  const appId = crypto.randomUUID();
  const storage = new IndexedDBStorage(appId, 'mutations');
  await storage.multiSet([
    ['a', { op: 'transact', order: 1 }],
    ['b', { op: 'transact', order: 2 }],
  ]);
  (await storage._dbPromise)?.close();
  const reopened = new IndexedDBStorage(appId, 'mutations');
  expect(await reopened.getItem('a')).toEqual({ op: 'transact', order: 1 });
  await reopened.removeItem('a');
  expect(await reopened.getItem('a')).toBeNull();
  expect(await reopened.getItem('b')).toEqual({ op: 'transact', order: 2 });
  (await reopened._dbPromise)?.close();
  indexedDB.deleteDatabase(`instant_${appId}_7`);
});

test('restricted browser can read and write through the memory fallback', async () => {
  vi.stubGlobal('indexedDB', undefined);
  const storage = new IndexedDBStorage(crypto.randomUUID(), 'mutations');
  await storage.multiSet([['pending', { op: 'transact' }]]);
  expect(await storage.getItem('pending')).toEqual({ op: 'transact' });
  await storage.removeItem('pending');
  expect(await storage.getItem('pending')).toBeNull();
});

test('devtool opens the configured self-hosted dashboard from its actual toggle', async () => {
  const dashURI = window.location.origin;
  createDevtool('test-app', {
    dashURI,
    position: 'bottom-right',
    allowedHosts: [window.location.hostname],
  });
  const toggle = document.querySelector<HTMLElement>(
    '.instant-devtool-toggler',
  )!;
  toggle.click();
  const frame = document.querySelector<HTMLIFrameElement>(
    '.instant-devtool-iframe',
  )!;
  expect(frame.src).toBe(`${dashURI}/_devtool?appId=test-app`);
  expect(frame.isConnected).toBe(true);
  toggle.click();
  expect(
    document.querySelector<HTMLElement>('.instant-devtool-container')!.style
      .display,
  ).toBe('none');
});
