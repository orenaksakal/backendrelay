import assert from 'node:assert/strict';
import { afterEach, test } from 'node:test';
import {
  jsonFetch,
  jsonMutate,
  cancelAuthenticatedRequests,
} from '../lib/fetch';
import {
  DashboardRequestError,
  isRetryableRead,
  requestError,
} from '../lib/request';
import {
  parseAuthInfo,
  readAuthInfo,
  persistAuthInfo,
} from '../lib/authSession';
import {
  memoryCacheProvider,
  clearMemoryCaches,
  removeLegacyResponseCaches,
} from '../lib/swrCache';
const originalFetch = globalThis.fetch;
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
afterEach(() => {
  globalThis.fetch = originalFetch;
  if (originalWindow)
    Object.defineProperty(globalThis, 'window', originalWindow);
  else Reflect.deleteProperty(globalThis, 'window');
});

test('JSON requests accept 201, 202 and empty 204 responses and preserve Headers', async () => {
  for (const status of [201, 202, 204]) {
    globalThis.fetch = async (_input, init) => {
      assert.equal(
        new Headers(init?.headers).get('authorization'),
        'Bearer test',
      );
      assert.equal(
        new Headers(init?.headers).get('x-backendrelay-source'),
        'dashboard',
      );
      return new Response(status === 204 ? null : '{"ok":true}', { status });
    };
    assert.deepEqual(
      await jsonFetch('/api', {
        headers: new Headers({ authorization: 'Bearer test' }),
      }),
      status === 204 ? undefined : { ok: true },
    );
  }
});

test('proxy HTML errors retain HTTP status and a safe request identifier', async () => {
  globalThis.fetch = async () =>
    new Response('<html>internal proxy diagnostics</html>', {
      status: 503,
      headers: { 'x-request-id': 'req_123', 'retry-after': '7' },
    });
  await assert.rejects(jsonFetch('/api'), (error: DashboardRequestError) => {
    assert.equal(error.status, 503);
    assert.equal(error.requestId, 'req_123');
    assert.equal(error.retryAfterMs, 7000);
    assert.equal(isRetryableRead(error), true);
    assert.match(requestError(error), /proxy.*Request req_123/);
    assert.equal(error.body, undefined);
    return true;
  });
});

test('unsafe request IDs and malformed successful bodies do not leak raw diagnostics', async () => {
  globalThis.fetch = async () =>
    new Response('<html>secret diagnostics</html>', {
      headers: { 'x-request-id': 'Bearer secret' },
    });
  await assert.rejects(jsonFetch('/api'), (error: DashboardRequestError) => {
    assert.equal(error.kind, 'response');
    assert.equal(error.requestId, undefined);
    assert.equal(isRetryableRead(error), false);
    assert.doesNotMatch(error.message, /secret/);
    return true;
  });
});

test('mutations preserve falsy bodies and never replay on network failure', async () => {
  let calls = 0;
  globalThis.fetch = async (_input, init) => {
    calls++;
    assert.equal(init?.body, 'false');
    throw new TypeError('network failure');
  };
  await assert.rejects(
    jsonMutate('/api', { token: 'test', body: false }),
    /Could not reach/,
  );
  assert.equal(calls, 1);
});

function pendingFetch() {
  globalThis.fetch = async (_input, init) =>
    new Promise((_resolve, reject) => {
      if (init?.signal?.aborted)
        reject(new DOMException('Aborted', 'AbortError'));
      init?.signal?.addEventListener(
        'abort',
        () => reject(new DOMException('Aborted', 'AbortError')),
        { once: true },
      );
    });
}
test('deadlines and caller cancellation remain distinguishable', async () => {
  pendingFetch();
  await assert.rejects(
    jsonFetch('/api', { timeoutMs: 5 }),
    (e: DashboardRequestError) => e.kind === 'timeout',
  );
  const controller = new AbortController();
  const request = jsonFetch('/api', { signal: controller.signal });
  controller.abort();
  await assert.rejects(request, { name: 'AbortError' });
});

test('retiring an account cancels its requests without cancelling the successor', async () => {
  pendingFetch();
  const old = jsonFetch('/old', { headers: { authorization: 'Bearer old' } });
  const successorController = new AbortController();
  const successor = jsonFetch('/new', {
    headers: { authorization: 'Bearer new' },
    signal: successorController.signal,
  });
  cancelAuthenticatedRequests('old');
  await assert.rejects(old, { name: 'AbortError' });
  successorController.abort();
  await assert.rejects(successor, { name: 'AbortError' });
});

test('malformed, disabled and full storage cannot break authentication', () => {
  assert.equal(parseAuthInfo('{bad').token, undefined);
  assert.equal(parseAuthInfo('{"token":1}').token, undefined);
  assert.equal(parseAuthInfo('{"token":"valid"}').token, 'valid');
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: {
      get localStorage() {
        throw new Error('blocked');
      },
    },
  });
  assert.equal(readAuthInfo().token, undefined);
  assert.doesNotThrow(() =>
    persistAuthInfo({ token: 'valid', user: undefined }),
  );
  assert.doesNotThrow(() => removeLegacyResponseCaches());
});

test('legacy response migration preserves preferences and clears every memory provider', () => {
  const values = new Map([
    ['swr-cache', 'secret'],
    ['dash:abcdef', 'secret'],
    ['darkMode', 'true'],
    ['workspace', 'personal'],
    ['@AUTH', '{}'],
  ]);
  removeLegacyResponseCaches({
    get length() {
      return values.size;
    },
    key: (i: number) => [...values.keys()][i],
    removeItem: (key: string) => values.delete(key),
  } as unknown as Storage);
  assert.deepEqual([...values.keys()], ['darkMode', 'workspace', '@AUTH']);
  const cache = memoryCacheProvider();
  cache.set('private', { data: 'secret' });
  clearMemoryCaches();
  assert.equal(cache.size, 0);
});

test('identical reads share transport but one subscriber cannot cancel another', async () => {
  let calls = 0;
  let transportSignal: AbortSignal | null | undefined;
  let finish: (response: Response) => void = () => {};
  globalThis.fetch = async (_input, init) => {
    calls++;
    transportSignal = init?.signal;
    return new Promise<Response>((resolve) => {
      finish = resolve;
    });
  };
  const first = new AbortController();
  const a = jsonFetch('/shared', { signal: first.signal });
  const b = jsonFetch('/shared');
  assert.equal(calls, 1);
  first.abort();
  await assert.rejects(a, { name: 'AbortError' });
  assert.equal(transportSignal?.aborted, false);
  finish(new Response('{"ok":true}'));
  assert.deepEqual(await b, { ok: true });
});

test('read deduplication isolates credentials and completed reads are not cached', async () => {
  let calls = 0;
  globalThis.fetch = async () => {
    calls++;
    return new Response('{}');
  };
  await Promise.all([
    jsonFetch('/scoped', { headers: { authorization: 'Bearer a' } }),
    jsonFetch('/scoped', { headers: { authorization: 'Bearer b' } }),
  ]);
  assert.equal(calls, 2);
  await jsonFetch('/scoped', { headers: { authorization: 'Bearer a' } });
  assert.equal(calls, 3);
});
