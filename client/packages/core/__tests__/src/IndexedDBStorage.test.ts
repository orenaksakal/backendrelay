import { afterEach, expect, test, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import IndexedDBStorage from '../../src/IndexedDBStorage';

afterEach(() => vi.unstubAllGlobals());

async function exerciseStorage() {
  const storage = new IndexedDBStorage('restricted-browser', 'kv');
  // Let initialization settle before reading, as Reactor does. There must be
  // no unhandled rejection even when the browser throws synchronously.
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(await storage.getItem('missing')).toBeNull();
  await storage.setItem('auth', { token: 'local' });
  await storage.multiSet([
    ['pending', [1, 2]],
    ['zero', 0],
  ]);
  expect(await storage.getItem('auth')).toEqual({ token: 'local' });
  expect(await storage.getItem('pending')).toEqual([1, 2]);
  expect(await storage.getItem('zero')).toBe(0);
  expect((await storage.getAllKeys()).sort()).toEqual([
    'auth',
    'pending',
    'zero',
  ]);
  await storage.removeItem('auth');
  expect(await storage.getItem('auth')).toBeNull();
  const otherStore = new IndexedDBStorage('restricted-browser', 'querySubs');
  const otherApp = new IndexedDBStorage('another-app', 'kv');
  expect(await otherStore.getItem('pending')).toBeNull();
  expect(await otherApp.getItem('pending')).toBeNull();
}

test('upstream #2779: missing IndexedDB falls back to usable memory storage', async () => {
  vi.stubGlobal('indexedDB', undefined);
  await exerciseStorage();
});

test('upstream #2779: a throwing global getter does not crash initialization', async () => {
  vi.stubGlobal('indexedDB', undefined);
  Object.defineProperty(globalThis, 'indexedDB', {
    configurable: true,
    get() {
      throw new DOMException('Storage denied', 'SecurityError');
    },
  });
  await exerciseStorage();
});

test('upstream #2779: synchronous open failure falls back', async () => {
  vi.stubGlobal('indexedDB', {
    open() {
      throw new DOMException('Storage denied', 'SecurityError');
    },
  });
  await exerciseStorage();
});

test('upstream #2779: asynchronous open failure falls back', async () => {
  vi.stubGlobal('indexedDB', {
    open() {
      const request = { onerror: null as null | ((event: Event) => void) };
      queueMicrotask(() => request.onerror?.(new Event('error')));
      return request;
    },
  });
  await exerciseStorage();
});

test('available IndexedDB still persists between storage instances', async () => {
  vi.stubGlobal('indexedDB', new IDBFactory());
  const first = new IndexedDBStorage('persistent-browser', 'kv');
  await first.setItem('auth', { token: 'persisted' });
  const second = new IndexedDBStorage('persistent-browser', 'kv');
  expect(await second.getItem('auth')).toEqual({ token: 'persisted' });
  (await first._dbPromise)?.close();
  (await second._dbPromise)?.close();
});

test('a write failure after successful initialization is not reported as persisted', async () => {
  vi.stubGlobal('indexedDB', new IDBFactory());
  const storage = new IndexedDBStorage('quota-browser', 'kv');
  const db = await storage._dbPromise;
  expect(db).not.toBeNull();
  const failure = new DOMException('Quota exceeded', 'QuotaExceededError');
  const transaction = vi.spyOn(db!, 'transaction').mockImplementation(() => {
    throw failure;
  });
  try {
    await expect(storage.setItem('pending', [1])).rejects.toBe(failure);
  } finally {
    transaction.mockRestore();
  }
  expect(await storage.getItem('pending')).toBeNull();
  db?.close();
});
