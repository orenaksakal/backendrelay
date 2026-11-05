import Cookies from 'js-cookie';
import { useContext, useSyncExternalStore } from 'react';
import useSwr, { SWRResponse } from 'swr';
import config from './config';
import { useReadScope } from './hooks/useReadScope';
import { cancelAuthenticatedRequests, jsonFetch, jsonMutate } from './fetch';
import { readRetryPolicy, DashboardRequestError } from './request';
import {
  emptyAuth,
  parseAuthInfo,
  readAuthInfo,
  persistAuthInfo,
  type AuthInfo,
} from './authSession';
import { clearMemoryCaches, removeLegacyResponseCaches } from './swrCache';
import type { SWRConfiguration } from 'swr';
import { TokenContext } from '@/lib/contexts';
import produce, { Draft } from 'immer';

// ----------
// Auth State

import type { AuthUser } from './authSession';

type Sub = () => void;
export type APIResponse<Data> = SWRResponse<Data> & {
  optimisticUpdate: <MutationResponse>(
    mutationPromiseToWaitFor: Promise<MutationResponse>,
    optimisticDataProducer?: (d: Draft<Data>) => void,
  ) => Promise<MutationResponse>;
};

function recordLoggedInStateInCookie(authInfo: AuthInfo) {
  try {
    Cookies.set('loggedIn', `${authInfo.token ? 1 : 0}`, { expires: 365 });
  } catch {}
}

removeLegacyResponseCaches();
let _AUTH_INFO = readAuthInfo();
let authGeneration = 0;
recordLoggedInStateInCookie(_AUTH_INFO);
const _SUBS = new Set<Sub>();
function subscribe(fn: Sub) {
  _SUBS.add(fn);
  return () => {
    _SUBS.delete(fn);
  };
}
function setAuthInfo(authInfo: AuthInfo, persist = true) {
  authGeneration += 1;
  if (
    _AUTH_INFO.token !== authInfo.token ||
    _AUTH_INFO.user?.id !== authInfo.user?.id
  ) {
    cancelAuthenticatedRequests(_AUTH_INFO.token);
    clearMemoryCaches();
  }
  _AUTH_INFO = { ...authInfo };
  if (persist) persistAuthInfo(_AUTH_INFO);
  recordLoggedInStateInCookie(_AUTH_INFO);
  _SUBS.forEach((fn) => fn());
}
if (typeof window !== 'undefined') {
  window.addEventListener('storage', (event) => {
    if (event.key === '@AUTH' || event.key === null) {
      setAuthInfo(
        event.key === null ? emptyAuth : parseAuthInfo(event.newValue),
        false,
      );
    }
  });
}
export function useAuthInfo(): AuthInfo {
  return useSyncExternalStore(
    subscribe,
    () => _AUTH_INFO,
    () => emptyAuth,
  );
}
export function useAuthToken(): string | undefined {
  return useAuthInfo().token;
}
function clearAuthInfo() {
  setAuthInfo(emptyAuth);
}

export function useAuthedFetch<Res = any>(
  path: string,
  options?: SWRConfiguration<Res>,
) {
  const token = useContext(TokenContext);
  return useTokenFetch<Res>(path, token, clearAuthInfo, options);
}

export function useAdmin() {
  const token = useAuthToken();
  const { data, error, isLoading } = useTokenFetch<{ ok: boolean }>(
    `${config.apiURI}/dash/check-admin`,
    token,
  );

  return {
    isAdmin: data?.ok === true,
    isLoading,
    error,
  };
}

export function useTokenFetch<Res>(
  path: string,
  token?: string,
  onUnauthorized?: () => void,
  options?: SWRConfiguration<Res>,
): APIResponse<Res> {
  const read = useReadScope(JSON.stringify([path, token]));
  const res = useSwr<Res, any, [string, string] | null>(
    path && token ? [path, token] : null,
    ([path, requestToken]) =>
      read(async (signal) => {
        try {
          return await jsonFetch<Res>(path, {
            signal,
            headers: { authorization: `Bearer ${requestToken}` },
          });
        } catch (error) {
          const failure = error as DashboardRequestError;
          const missingUser =
            failure.status === 400 &&
            failure.body?.type === 'record-not-found' &&
            failure.body?.hint?.['record-type'] === 'instant-user';
          // A delayed failure from a retired account must not log out its successor.
          if (
            (failure.status === 401 || missingUser) &&
            _AUTH_INFO.token === requestToken
          )
            onUnauthorized?.();
          throw error;
        }
      }),
    { ...readRetryPolicy, ...options, keepPreviousData: false },
  );

  return {
    ...res,
    data: [401, 403].includes(res.error?.status) ? undefined : res.data,
    optimisticUpdate: (
      mutationPromiseToWaitFor,
      optimisticDataProducer,
    ): any => {
      return optimisticUpdate(
        res,
        mutationPromiseToWaitFor,
        optimisticDataProducer,
      );
    },
  };
}

// --------
// Auth API

export function sendMagicCode({ email }: { email: string }) {
  return jsonFetch(`${config.apiURI}/dash/auth/send_magic_code`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email }),
  });
}

export async function verifyMagicCode({
  email,
  code,
}: {
  email: string;
  code: string;
}) {
  const generation = authGeneration;
  const res: { token: string; user: AuthUser } = await jsonFetch(
    `${config.apiURI}/dash/auth/verify_magic_code`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ email, code }),
    },
  );
  if (generation !== authGeneration)
    throw new Error('Your session changed while signing in. Please try again.');
  setAuthInfo({ token: res.token, user: res.user });
  return res;
}

export async function signOut() {
  const token = _AUTH_INFO.token;
  clearAuthInfo();
  try {
    if (token) {
      await jsonFetch(`${config.apiURI}/dash/signout`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({}),
      });
    }
  } catch (e) {
    console.error('Error signing out', e);
  }
}

// ---------
// OAuth API

export async function exchangeOAuthCodeForToken({ code }: { code: string }) {
  const generation = authGeneration;
  const res: { token: string; redirect_path: string; user: AuthUser } =
    await jsonFetch(`${config.apiURI}/dash/oauth/token`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ code }),
    });
  if (generation !== authGeneration)
    throw new Error('Your session changed while signing in. Please try again.');
  setAuthInfo({ token: res.token, user: res.user });
  return res;
}

export async function claimTicket({
  ticket,
  token,
}: {
  ticket: string;
  token: string;
}) {
  return jsonMutate(`${config.apiURI}/dash/cli/auth/claim`, {
    token,
    body: { ticket },
  });
}

export async function voidTicket({
  ticket,
  token,
}: {
  ticket: string;
  token: string;
}) {
  return jsonMutate(`${config.apiURI}/dash/cli/auth/void`, {
    token,
    body: { ticket },
  });
}

/**
 * Abstracts over the common pattern of optimistically updating an SWR response during a mutation call
 * Takes an SWR response, a mutation action, and an Immer producer that generates optimistic data
 * It handles:
 * 1. Updating the SWR cache with the optimistic data
 * 2. Revalidating the SWR response after the mutation
 * 3. Rolling back the cache if the mutation fails
 *
 * Context:
 * - https://swr.vercel.app/docs/mutation#optimistic-updates
 * - https://swr.vercel.app/docs/mutation#update-cache-after-mutation
 */
export function optimisticUpdate<T>(
  swrResponse: SWRResponse<T>,
  mutationPromiseToWaitFor: Promise<any>,
  optimisticDataProducer?: (d: Draft<T>) => any,
): Promise<T | undefined> {
  return swrResponse.mutate(
    // wait on action, then re-fetch swrResponse
    mutationPromiseToWaitFor,
    {
      // Restore the cache to its previous state if the mutation throws
      rollbackOnError: true,
      // SWR will write the result of the action to the cache by default, which we don't want
      // because our mutation responses aren't the same shape as our query responses
      // https://swr.vercel.app/docs/mutation#update-cache-after-mutation
      populateCache: false,
      // Optimistic update helper with immer
      // https://swr.vercel.app/docs/mutation#optimistic-updates
      optimisticData: (currentValue) => {
        if (currentValue && optimisticDataProducer) {
          return produce(currentValue, optimisticDataProducer);
        }

        return currentValue as T;
      },
    },
  );
}
