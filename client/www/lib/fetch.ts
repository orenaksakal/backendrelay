import { DashboardRequestError } from './request';
import config from './config';
import { routeDashboardURL } from './dashboardApi';

const trackingHeaders = { 'X-BackendRelay-Source': 'dashboard' };
const activeRequests = new Map<AbortController, string | null>();

/** Drop outstanding responses from a retired authentication session. */
export function cancelAuthenticatedRequests(token?: string) {
  for (const [controller, authorization] of activeRequests) {
    if (authorization && (!token || authorization === `Bearer ${token}`)) {
      controller.abort();
    }
  }
}

export type JsonRequestInit = RequestInit & { timeoutMs?: number };

export function dashboardFetch(input: RequestInfo, init?: RequestInit) {
  const original = input instanceof Request ? input.url : input;
  const routed = routeDashboardURL(
    original,
    config.apiURI,
    config.dashboardApiURI ?? config.apiURI,
  );
  const request =
    routed === original
      ? input
      : input instanceof Request
        ? new Request(routed, input)
        : routed;
  return fetch(request, init);
}

async function fetchJson<T = any>(
  input: RequestInfo,
  init?: JsonRequestInit,
): Promise<T> {
  const { timeoutMs = 30_000, ...options } = init ?? {};
  const headers = new Headers(
    input instanceof Request ? input.headers : undefined,
  );
  new Headers(options.headers).forEach((value, key) => headers.set(key, value));
  Object.entries(trackingHeaders).forEach(([key, value]) =>
    headers.set(key, value),
  );
  const controller = new AbortController();
  const signal =
    options.signal ?? (input instanceof Request ? input.signal : undefined);
  const abort = () => controller.abort(signal?.reason);
  if (signal?.aborted) abort();
  else signal?.addEventListener('abort', abort, { once: true });
  activeRequests.set(controller, headers.get('authorization'));
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);
  try {
    const response = await dashboardFetch(input, {
      ...options,
      headers,
      signal: controller.signal,
    });
    const rawId = response.headers.get('x-request-id');
    const requestId =
      rawId && /^[A-Za-z0-9._:-]{1,128}$/.test(rawId) ? rawId : undefined;
    const text = await response.text();
    let body: any;
    try {
      body = text ? JSON.parse(text) : undefined;
    } catch {
      if (response.ok)
        throw new DashboardRequestError(
          'The server returned an unreadable response. Refresh or contact your operator.',
          response.status,
          undefined,
          requestId,
          'response',
        );
    }
    if (!response.ok) {
      const retryHeader = response.headers.get('retry-after');
      const retryAfter =
        retryHeader == null
          ? undefined
          : /^\d+$/.test(retryHeader)
            ? Number(retryHeader) * 1000
            : Date.parse(retryHeader) - Date.now();
      const message =
        typeof body?.message === 'string'
          ? body.message
          : response.status >= 500
            ? 'The service or proxy is unavailable. Try again; contact your operator if this continues.'
            : `Request failed (HTTP ${response.status}). Try again.`;
      throw new DashboardRequestError(
        message,
        response.status,
        body,
        requestId,
        'http',
        retryAfter !== undefined && Number.isFinite(retryAfter)
          ? Math.max(0, retryAfter)
          : undefined,
      );
    }
    return body as T;
  } catch (error) {
    if (timedOut)
      throw new DashboardRequestError(
        'The request timed out. Check the connection or service status before trying again.',
        0,
        undefined,
        undefined,
        'timeout',
      );
    if (controller.signal.aborted)
      throw new DOMException('Request cancelled', 'AbortError');
    if (error instanceof DashboardRequestError) throw error;
    throw new DashboardRequestError(
      'Could not reach the service. Check your connection and the configured API or proxy address.',
      0,
      undefined,
      undefined,
      'network',
    );
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', abort);
    activeRequests.delete(controller);
  }
}

type SharedRead = {
  controller: AbortController;
  promise: Promise<any>;
  consumers: number;
};
const reads = new Map<string, SharedRead>();

/** Share identical reads; each consumer retains its own cancellation lifetime. */
export function jsonFetch<T = any>(
  input: RequestInfo,
  init?: JsonRequestInit,
): Promise<T> {
  if (
    typeof input !== 'string' ||
    (init?.method ?? 'GET').toUpperCase() !== 'GET'
  ) {
    return fetchJson<T>(input, init);
  }
  if (init?.signal?.aborted)
    return Promise.reject(new DOMException('Request cancelled', 'AbortError'));
  const headers = [...new Headers(init?.headers).entries()].sort(([a], [b]) =>
    a.localeCompare(b),
  );
  const key = JSON.stringify([
    input,
    headers,
    init?.timeoutMs ?? 30_000,
    init?.credentials,
    init?.cache,
    init?.mode,
    init?.redirect,
    init?.integrity,
  ]);
  let read = reads.get(key);
  if (!read) {
    const controller = new AbortController();
    read = {
      controller,
      consumers: 0,
      promise: fetchJson<T>(input, { ...init, signal: controller.signal }),
    };
    reads.set(key, read);
  }
  const shared = read;
  shared.consumers++;
  return new Promise<T>((resolve, reject) => {
    let finished = false;
    const release = () => {
      if (finished) return false;
      finished = true;
      init?.signal?.removeEventListener('abort', abort);
      if (--shared.consumers === 0) {
        if (reads.get(key) === shared) reads.delete(key);
        shared.controller.abort();
      }
      return true;
    };
    const abort = () => {
      if (release())
        reject(new DOMException('Request cancelled', 'AbortError'));
    };
    init?.signal?.addEventListener('abort', abort, { once: true });
    shared.promise.then(
      (value) => {
        if (release()) resolve(value);
      },
      (error) => {
        if (release()) reject(error);
      },
    );
  });
}

export async function jsonMutate<T>(
  input: RequestInfo,
  {
    token,
    body,
    method,
  }: { token: string; body?: any; method?: 'POST' | 'DELETE' },
): Promise<T> {
  return jsonFetch(input, {
    method: method ?? 'POST',
    headers: {
      authorization: `Bearer ${token}`,
      'content-type': 'application/json',
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}
export { trackingHeaders };
