import { act, renderHook } from '@testing-library/react';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { z } from 'zod';
import { apiFetch, getApiToken } from '@/lib/api/client';
import { defineEndpoint } from '@/lib/api/endpoints/types';
import { createQueryClient } from '@/lib/query-client';
import { loadRuntimeConfig, resetRuntimeConfig } from '@/lib/runtime-config';
import { server } from '@/test/msw/node';
import {
  LEGACY_SESSION_STORAGE_KEY,
  SESSION_STORAGE_KEY,
  getSession,
  loginRedirectPath,
  parseStoredSession,
  readStoredSession,
  registerSessionCleanup,
  resetSession,
  setLoginNavigation,
  setSession,
  signOut,
  startSession,
  useSession,
  type Session,
} from './session';

const pingEndpoint = defineEndpoint({
  method: 'GET',
  path: '/dash/apps/:appId/ping',
  response: z.looseObject({ ok: z.boolean() }),
});

const session: Session = {
  token: 'session-token',
  user: { id: 'user-1', email: 'operator@example.test' },
};

let navigateToLogin: (path: string) => void;

beforeEach(async () => {
  window.localStorage.clear();
  window.history.replaceState(null, '', '/');
  resetSession();
  resetRuntimeConfig();
  await loadRuntimeConfig();
  navigateToLogin = vi.fn();
  setLoginNavigation(navigateToLogin);
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.history.replaceState(null, '', '/');
});

describe('legacy session migration', () => {
  it('copies @AUTH into br.session and keeps the old key', () => {
    const legacy = {
      token: session.token,
      user: { ...session.user, created_at: '2026-01-01T00:00:00Z' },
    };
    window.localStorage.setItem(
      LEGACY_SESSION_STORAGE_KEY,
      JSON.stringify(legacy),
    );

    startSession();

    expect(getSession()).toMatchObject({ token: session.token });
    expect(getSession()?.user.email).toBe(session.user.email);
    expect(getApiToken()).toBe(session.token);
    const stored = window.localStorage.getItem(SESSION_STORAGE_KEY);
    expect(stored).not.toBeNull();
    expect(parseStoredSession(stored)?.token).toBe(session.token);
    expect(
      window.localStorage.getItem(LEGACY_SESSION_STORAGE_KEY),
    ).not.toBeNull();
  });

  it('keeps br.session when both keys are present', () => {
    window.localStorage.setItem(
      SESSION_STORAGE_KEY,
      JSON.stringify({ ...session, token: 'current-token' }),
    );
    window.localStorage.setItem(
      LEGACY_SESSION_STORAGE_KEY,
      JSON.stringify({ ...session, token: 'stale-token' }),
    );

    startSession();

    expect(getSession()?.token).toBe('current-token');
    expect(getApiToken()).toBe('current-token');
  });

  it('ignores a legacy entry that does not hold a usable session', () => {
    window.localStorage.setItem(LEGACY_SESSION_STORAGE_KEY, '{"token":""}');

    startSession();

    expect(getSession()).toBeNull();
    expect(readStoredSession()).toBeNull();
    expect(window.localStorage.getItem(SESSION_STORAGE_KEY)).toBeNull();
  });
});

describe('cross-tab sync', () => {
  it('signs this tab in when another tab signs in', () => {
    startSession();
    expect(getSession()).toBeNull();

    act(() => {
      window.dispatchEvent(
        new StorageEvent('storage', {
          key: SESSION_STORAGE_KEY,
          newValue: JSON.stringify(session),
        }),
      );
    });

    expect(getSession()?.user.email).toBe(session.user.email);
    expect(getApiToken()).toBe(session.token);
  });

  it('clears the session, the token and the caches when another tab signs out', () => {
    startSession();
    setSession(session);
    const cleanup = vi.fn();
    const unregister = registerSessionCleanup(cleanup);
    expect(getSession()).not.toBeNull();

    act(() => {
      window.dispatchEvent(
        new StorageEvent('storage', {
          key: SESSION_STORAGE_KEY,
          newValue: null,
        }),
      );
    });

    expect(getSession()).toBeNull();
    expect(getApiToken()).toBeNull();
    expect(cleanup).toHaveBeenCalledTimes(1);
    unregister();
  });

  it('notifies mounted components', () => {
    startSession();
    const { result } = renderHook(() => useSession());
    expect(result.current).toBeNull();

    act(() => {
      window.dispatchEvent(
        new StorageEvent('storage', {
          key: SESSION_STORAGE_KEY,
          newValue: JSON.stringify(session),
        }),
      );
    });

    expect(result.current?.user.email).toBe(session.user.email);
  });
});

describe('sign-out', () => {
  it('empties the query cache and both storage keys', () => {
    startSession();
    setSession(session);
    window.localStorage.setItem(
      LEGACY_SESSION_STORAGE_KEY,
      JSON.stringify(session),
    );
    const client = createQueryClient();
    client.setQueryData(['apps'], [{ id: 'app-1' }]);

    signOut();

    expect(client.getQueryData(['apps'])).toBeUndefined();
    expect(getSession()).toBeNull();
    expect(getApiToken()).toBeNull();
    expect(window.localStorage.getItem(SESSION_STORAGE_KEY)).toBeNull();
    expect(window.localStorage.getItem(LEGACY_SESSION_STORAGE_KEY)).toBeNull();
  });
});

describe('401 handling', () => {
  it('clears the session and redirects to the login screen', async () => {
    window.history.replaceState(null, '', '/apps/app-1/explorer?tab=schema');
    startSession();
    setSession(session);
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          { type: 'record-not-found', message: 'Your session expired.' },
          { status: 401 },
        ),
      ),
    );

    await apiFetch(pingEndpoint, { params: { appId: 'app-1' } }).catch(
      () => undefined,
    );

    expect(navigateToLogin).toHaveBeenCalledWith(
      '/login?redirect=%2Fapps%2Fapp-1%2Fexplorer%3Ftab%3Dschema',
    );
    expect(getSession()).toBeNull();
    expect(getApiToken()).toBeNull();
    expect(window.localStorage.getItem(SESSION_STORAGE_KEY)).toBeNull();
  });

  it('leaves a sign-in failure on the form', async () => {
    startSession();
    server.use(
      http.get('http://localhost:8888/dash/apps/:appId/ping', () =>
        HttpResponse.json(
          { type: 'record-not-found', message: 'That code is not valid.' },
          { status: 401 },
        ),
      ),
    );

    await apiFetch(pingEndpoint, { params: { appId: 'app-1' } }).catch(
      () => undefined,
    );

    expect(navigateToLogin).not.toHaveBeenCalled();
    expect(getSession()).toBeNull();
  });

  it('only accepts same-origin redirect targets', () => {
    expect(loginRedirectPath('/apps/app-1')).toBe(
      '/login?redirect=%2Fapps%2Fapp-1',
    );
    expect(loginRedirectPath('//evil.example')).toBe('/login?redirect=%2F');
    expect(loginRedirectPath('https://evil.example')).toBe(
      '/login?redirect=%2F',
    );
  });
});
