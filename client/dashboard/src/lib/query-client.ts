import {
  MutationCache,
  QueryClient,
  type QueryKey,
} from '@tanstack/react-query';
import { isApiError } from './api/client';
import { registerSessionCleanup } from './auth/session';
import { showErrorToast } from './toast';

/** Lists stay fresh for 30s; the plan's polling screens opt in explicitly. */
export const QUERY_STALE_TIME_MS = 30_000;

/** One retry for transport/5xx failures, none for 4xx or timeouts. */
export const QUERY_RETRY_LIMIT = 1;

/** Mutations declare the keys they invalidate through `meta.invalidates`. */
export interface MutationMeta {
  invalidates?: QueryKey[];
}

export function shouldRetryQuery(
  failureCount: number,
  error: unknown,
): boolean {
  if (isApiError(error)) {
    if (error.status >= 400 && error.status < 500) return false;
    if (error.type === 'timeout') return false;
  }
  return failureCount < QUERY_RETRY_LIMIT;
}

/**
 * Builds the app QueryClient: 30s `staleTime`, no retry on 4xx, one global
 * mutation-error toast, and `meta.invalidates` handled once for every mutation.
 */
export function createQueryClient(): QueryClient {
  // The cache callbacks need the client, which only exists after the cache is
  // constructed. The indirection keeps the closure honest without assertions.
  const clientRef: { current?: QueryClient } = {};

  const mutationCache = new MutationCache({
    onError: (error) => {
      showErrorToast(error);
    },
    onSuccess: (_data, _variables, _onMutateResult, mutation) => {
      const client = clientRef.current;
      if (client === undefined) return;
      const invalidates = mutation.options.meta?.['invalidates'] as
        | QueryKey[]
        | undefined;
      if (invalidates === undefined || invalidates.length === 0) return;
      for (const queryKey of invalidates) {
        void client.invalidateQueries({ queryKey });
      }
    },
  });

  const client = new QueryClient({
    mutationCache,
    defaultOptions: {
      queries: {
        staleTime: QUERY_STALE_TIME_MS,
        retry: shouldRetryQuery,
        refetchOnWindowFocus: false,
      },
      mutations: { retry: false },
    },
  });
  clientRef.current = client;
  // Nothing cached under one session may survive sign-out (D3.2).
  registerSessionCleanup(() => {
    client.clear();
  });
  return client;
}

let sharedQueryClient: QueryClient | undefined;

/**
 * The browser's single QueryClient. Route loaders and the root provider share
 * it, so a loader-warmed query is already rendered by the time the screen
 * mounts. `createQueryClient()` stays available for isolated tests.
 */
export function getQueryClient(): QueryClient {
  sharedQueryClient ??= createQueryClient();
  return sharedQueryClient;
}
