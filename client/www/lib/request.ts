import type { SWRConfiguration } from 'swr';

/** Shared dashboard request contract. Mutations are never automatically replayed. */
export type Request = <T>(path: string, init?: RequestInit) => Promise<T>;
export type RequestFailure = 'http' | 'network' | 'timeout' | 'response';

export class DashboardRequestError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body: any,
    readonly requestId?: string,
    readonly kind: RequestFailure = 'http',
    readonly retryAfterMs?: number,
  ) {
    super(message);
    this.name = 'DashboardRequestError';
  }
}

export function requestError(error: unknown): string {
  if (error instanceof DashboardRequestError) {
    return `${error.message}${error.requestId ? ` (Request ${error.requestId})` : ''}`;
  }
  const value = error as { body?: { message?: string }; message?: string };
  return value?.body?.message || value?.message || 'Request failed. Try again.';
}

export function isRetryableRead(error: unknown): boolean {
  return (
    error instanceof DashboardRequestError &&
    (error.kind === 'network' ||
      error.kind === 'timeout' ||
      [408, 429, 502, 503, 504].includes(error.status))
  );
}

export const readRetryPolicy: SWRConfiguration = {
  keepPreviousData: false,
  errorRetryCount: 2,
  errorRetryInterval: 3000,
  refreshWhenHidden: false,
  refreshWhenOffline: false,
  shouldRetryOnError: isRetryableRead,
  onErrorRetry(error, _key, _config, revalidate, { retryCount }) {
    if (!isRetryableRead(error) || retryCount > 2) return;
    const available = () =>
      typeof document === 'undefined' ||
      (!document.hidden && navigator.onLine !== false);
    if (!available()) return;
    const delay = Math.max(
      3000 * 2 ** (retryCount - 1),
      Math.min(
        2_147_483_647,
        (error as DashboardRequestError).retryAfterMs ?? 0,
      ),
    );
    setTimeout(() => {
      if (available()) void revalidate({ retryCount });
    }, delay);
  },
};
