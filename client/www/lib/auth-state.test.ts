// @vitest-environment jsdom
import React, { createElement } from 'react';
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
vi.mock('./config', () => ({ default: { apiURI: 'http://backend.test' } }));
vi.mock('js-cookie', () => ({ default: { set: vi.fn() } }));
import { useAuthInfo, verifyMagicCode, signOut } from './auth';

function Account() {
  const auth = useAuthInfo();
  return createElement(
    'div',
    null,
    auth.user?.email ?? auth.token ?? 'Signed out',
  );
}
afterEach(async () => {
  cleanup();
  await act(async () =>
    window.dispatchEvent(
      new StorageEvent('storage', { key: '@AUTH', newValue: '{}' }),
    ),
  );
  vi.unstubAllGlobals();
});
test('login and token rotation publish immutable snapshots without a reload', async () => {
  render(createElement(Account));
  expect(screen.getByText('Signed out')).toBeTruthy();
  vi.stubGlobal(
    'fetch',
    vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            token: 'a',
            user: { id: 'a', email: 'a@example.test', created_at: 'now' },
          }),
        ),
    ),
  );
  await act(async () => {
    await verifyMagicCode({ email: 'a@example.test', code: '123456' });
  });
  expect(screen.getByText('a@example.test')).toBeTruthy();
  await act(async () =>
    window.dispatchEvent(
      new StorageEvent('storage', {
        key: '@AUTH',
        newValue: JSON.stringify({
          token: 'b',
          user: { id: 'b', email: 'b@example.test', created_at: 'now' },
        }),
      }),
    ),
  );
  expect(screen.getByText('b@example.test')).toBeTruthy();
  expect(screen.queryByText('a@example.test')).toBeNull();
});
test('cross-tab logout and clear hide account content synchronously', async () => {
  render(createElement(Account));
  await act(async () =>
    window.dispatchEvent(
      new StorageEvent('storage', {
        key: '@AUTH',
        newValue: '{"token":"session"}',
      }),
    ),
  );
  expect(screen.getByText('session')).toBeTruthy();
  await act(async () =>
    window.dispatchEvent(new StorageEvent('storage', { key: null })),
  );
  expect(screen.getByText('Signed out')).toBeTruthy();
});
test('local logout hides account data before a stalled signout request completes', async () => {
  render(createElement(Account));
  await act(async () =>
    window.dispatchEvent(
      new StorageEvent('storage', {
        key: '@AUTH',
        newValue: '{"token":"session"}',
      }),
    ),
  );
  let finish!: (response: Response) => void;
  vi.stubGlobal(
    'fetch',
    vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          finish = resolve;
        }),
    ),
  );
  let logout!: Promise<void>;
  await act(async () => {
    logout = signOut();
  });
  expect(screen.getByText('Signed out')).toBeTruthy();
  await act(async () => {
    finish(new Response('{}'));
    await logout;
  });
});

test('development remounts do not poison the SWR request with a cancellation', async () => {
  const { createRoot } = await import('react-dom/client');
  const { SWRConfig } = await import('swr');
  const { useTokenFetch } = await import('./auth');
  const host = document.createElement('div');
  document.body.append(host);
  const root = createRoot(host);
  const network = vi.fn(async (_input, init) => {
    await Promise.resolve();
    if (init?.signal?.aborted) throw new DOMException('Aborted', 'AbortError');
    return new Response('{"label":"Ready after remount"}');
  });
  vi.stubGlobal('fetch', network);
  function Resource() {
    const value = useTokenFetch<{ label: string }>(
      'http://backend.test/dash',
      'remount-session',
    );
    return createElement(
      'div',
      null,
      value.error?.message ?? value.data?.label ?? 'Loading',
    );
  }
  try {
    await act(async () =>
      root.render(
        createElement(
          React.StrictMode,
          null,
          createElement(
            SWRConfig,
            { value: { provider: () => new Map() } },
            createElement(Resource),
          ),
        ),
      ),
    );
    expect(host.textContent).toContain('Ready after remount');
    expect(host.textContent).not.toContain('cancelled');
  } finally {
    await act(async () => root.unmount());
    host.remove();
  }
});

test('unmounting the SWR fetch leader keeps the read alive for its remaining subscriber', async () => {
  const { SWRConfig } = await import('swr');
  const { useTokenFetch } = await import('./auth');
  let signal: AbortSignal | null | undefined;
  let finish!: (response: Response) => void;
  const network = vi.fn((_input, init) => {
    signal = init.signal;
    return new Promise<Response>((resolve, reject) => {
      finish = resolve;
      signal?.addEventListener('abort', () =>
        reject(new DOMException('Aborted', 'AbortError')),
      );
    });
  });
  vi.stubGlobal('fetch', network);
  function Resource({ name }: { name: string }) {
    const value = useTokenFetch<{ label: string }>(
      'http://backend.test/shared',
      'shared-session',
    );
    return createElement(
      'div',
      null,
      `${name}:${value.error?.message ?? value.data?.label ?? 'Loading'}`,
    );
  }
  const cache = new Map();
  function Pair({ first }: { first: boolean }) {
    return createElement(
      SWRConfig,
      { value: { provider: () => cache } },
      first && createElement(Resource, { key: 'first', name: 'First' }),
      createElement(Resource, { key: 'second', name: 'Second' }),
    );
  }
  const view = render(createElement(Pair, { first: true }));
  expect(network).toHaveBeenCalledTimes(1);
  await act(async () => view.rerender(createElement(Pair, { first: false })));
  expect(signal?.aborted).toBe(false);
  await act(async () => finish(new Response('{"label":"Shared result"}')));
  expect(screen.getByText('Second:Shared result')).toBeTruthy();
});

test('the last SWR subscriber cancels the transport on unmount', async () => {
  const { SWRConfig } = await import('swr');
  const { useTokenFetch } = await import('./auth');
  let signal: AbortSignal | null | undefined;
  vi.stubGlobal(
    'fetch',
    vi.fn((_input, init) => {
      signal = init.signal;
      return new Promise<Response>((_resolve, reject) =>
        signal?.addEventListener('abort', () =>
          reject(new DOMException('Aborted', 'AbortError')),
        ),
      );
    }),
  );
  function Resource() {
    useTokenFetch('http://backend.test/retired', 'retired-session');
    return null;
  }
  const view = render(
    createElement(
      SWRConfig,
      { value: { provider: () => new Map() } },
      createElement(Resource),
    ),
  );
  await act(async () => view.unmount());
  expect(signal?.aborted).toBe(true);
});

test.each(['magic', 'oauth'] as const)(
  'late %s login cannot replace a cross-tab account transition',
  async (method) => {
    render(createElement(Account));
    let finish!: (response: Response) => void;
    vi.stubGlobal(
      'fetch',
      vi.fn(
        () =>
          new Promise<Response>((resolve) => {
            finish = resolve;
          }),
      ),
    );
    const { exchangeOAuthCodeForToken } = await import('./auth');
    const pending =
      method === 'magic'
        ? verifyMagicCode({ email: 'old@example.test', code: '123456' })
        : exchangeOAuthCodeForToken({ code: 'old-code' });
    const rejected = expect(pending).rejects.toThrow('session changed');
    await act(async () =>
      window.dispatchEvent(
        new StorageEvent('storage', {
          key: '@AUTH',
          newValue: '{"token":"successor"}',
        }),
      ),
    );
    await act(async () => {
      finish(
        new Response(
          JSON.stringify({
            token: 'old-session',
            user: { id: 'old', email: 'old@example.test', created_at: 'now' },
          }),
        ),
      );
      await rejected;
    });
    expect(screen.getByText('successor')).toBeTruthy();
    expect(screen.queryByText('old@example.test')).toBeNull();
  },
);
