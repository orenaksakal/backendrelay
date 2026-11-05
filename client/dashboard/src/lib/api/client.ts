import { z } from 'zod';
import { getRuntimeConfig, type RuntimeConfig } from '@/lib/runtime-config';
import type { ApiParams, Endpoint } from './endpoints/types';

/** Every dashboard request carries the surface it came from. */
export const API_SOURCE_HEADER = 'X-BackendRelay-Source';
export const API_SOURCE = 'dashboard';

/** Requests that outlive this limit fail with an `ApiError` instead of hanging. */
export const API_TIMEOUT_MS = 30_000;

/** Management traffic can be served from a private gateway origin. */
const GATEWAY_PREFIXES = ['/dash', '/superadmin', '/platform/oauth'];

const errorBodySchema = z.looseObject({
  type: z.string().optional(),
  message: z.string().optional(),
  hint: z.unknown().optional(),
  'trace-id': z.string().optional(),
  error: z.string().optional(),
});

let apiToken: string | null = null;

/**
 * Sets the bearer token for later requests. D3.2 calls this whenever the
 * session is loaded, refreshed or cleared.
 */
export function setApiToken(token: string | null): void {
  apiToken = token;
}

export function getApiToken(): string | null {
  return apiToken;
}

/** Runs once for every 401 response; the session layer installs the handler. */
export type UnauthorizedHandler = () => void;

let unauthorizedHandler: UnauthorizedHandler | null = null;

/**
 * Wires the session layer in (D3.2). The handler clears the session and sends
 * the tab to `/login`; `apiFetch` still throws so in-flight callers can settle.
 */
export function setUnauthorizedHandler(
  handler: UnauthorizedHandler | null,
): void {
  unauthorizedHandler = handler;
}

export interface ApiErrorInit {
  /** HTTP status, or `0` for transport and client-side failures. */
  status: number;
  type: string;
  message: string;
  hint?: unknown;
  requestId?: string;
}

/** The single failure shape every API caller can rely on. */
export class ApiError extends Error {
  readonly status: number;
  readonly type: string;
  readonly hint?: unknown;
  readonly requestId?: string;

  constructor(init: ApiErrorInit) {
    super(init.message);
    this.name = 'ApiError';
    this.status = init.status;
    this.type = init.type;
    this.hint = init.hint;
    this.requestId = init.requestId;
  }
}

export function isApiError(error: unknown): error is ApiError {
  return error instanceof ApiError;
}

export interface ApiRequestOptions<TBody> {
  params?: ApiParams;
  body?: TBody;
  signal?: AbortSignal;
  /** Overrides `API_TIMEOUT_MS` for lookups with their own deadline. */
  timeoutMs?: number;
  /**
   * Path parameters whose `/` stays a path separator. Webhook ISNs look like
   * `0/328/48953748` and the server routes them through a wildcard segment.
   */
  slashedPathParams?: readonly string[];
}

/** Resolves the origin for a path: management routes prefer the gateway. */
export function apiBaseUrlFor(path: string, config: RuntimeConfig): string {
  if (config.dashboardApiUrl === undefined) return config.apiBaseUrl;
  const viaGateway = GATEWAY_PREFIXES.some(
    (prefix) => path === prefix || path.startsWith(`${prefix}/`),
  );
  return viaGateway ? config.dashboardApiUrl : config.apiBaseUrl;
}

/**
 * Fills `:name` segments from `params` and turns every remaining parameter into
 * a query parameter. Returns an absolute URL. A name listed in
 * `slashedPathParams` keeps `/` as a separator while each part is escaped.
 */
export function buildApiUrl(
  path: string,
  params: ApiParams | undefined,
  config: RuntimeConfig,
  slashedPathParams: readonly string[] = [],
): string {
  const used = new Set<string>();
  const resolved = path.replace(/:([A-Za-z0-9_]+)/g, (_match, name: string) => {
    const value = params?.[name];
    if (value === undefined || value === null || value === '') {
      throw new ApiError({
        status: 0,
        type: 'param-missing',
        message: `Missing path parameter "${name}".`,
      });
    }
    used.add(name);
    const text = String(value);
    return slashedPathParams.includes(name)
      ? encodeSlashedPathParam(name, text)
      : encodeURIComponent(text);
  });

  const url = new URL(apiBaseUrlFor(resolved, config));
  url.pathname = `${url.pathname.replace(/\/$/, '')}${resolved}`;
  for (const [key, value] of Object.entries(params ?? {})) {
    if (used.has(key) || value === undefined || value === null) continue;
    url.searchParams.append(key, String(value));
  }
  return url.toString();
}

/** Maps a JSON body + headers onto the typed error contract. */
export function apiErrorFromResponse(
  status: number,
  body: unknown,
  headerRequestId?: string,
): ApiError {
  const parsed = errorBodySchema.safeParse(body);
  const data = parsed.success ? parsed.data : undefined;
  const message = data?.message ?? data?.error ?? fallbackErrorMessage(status);
  return new ApiError({
    status,
    type: data?.type ?? 'http-error',
    message,
    hint: data?.hint,
    requestId:
      requestIdValue(data?.['trace-id']) ?? requestIdValue(headerRequestId),
  });
}

/**
 * Fetches an endpoint, parses its response against the endpoint schema and
 * throws `ApiError` for HTTP, transport, timeout and shape failures. Caller
 * aborts are rethrown unchanged so TanStack Query can treat them as cancels.
 */
export async function apiFetch<
  TResponse extends z.ZodType,
  TBody extends z.ZodType | undefined = undefined,
>(
  endpoint: Endpoint<TResponse, TBody>,
  options: ApiRequestOptions<
    TBody extends z.ZodType ? z.input<TBody> : never
  > = {},
): Promise<z.output<TResponse>> {
  const config = getRuntimeConfig();
  if (config === undefined) {
    throw new ApiError({
      status: 0,
      type: 'configuration',
      message: 'The dashboard configuration has not loaded yet.',
    });
  }

  const url = buildApiUrl(
    endpoint.path,
    options.params,
    config,
    options.slashedPathParams,
  );
  const headers = new Headers({ accept: 'application/json' });
  headers.set(API_SOURCE_HEADER, API_SOURCE);
  if (apiToken !== null && apiToken !== '') {
    headers.set('authorization', `Bearer ${apiToken}`);
  }
  const { payload, contentType } = serializeBody(options.body);
  if (contentType !== null) headers.set('content-type', contentType);

  const controller = new AbortController();
  const timedOut = { value: false };
  const forwardAbort = () => {
    controller.abort(options.signal?.reason);
  };
  if (options.signal?.aborted === true) {
    forwardAbort();
  } else {
    options.signal?.addEventListener('abort', forwardAbort, { once: true });
  }
  const timer = setTimeout(() => {
    timedOut.value = true;
    controller.abort();
  }, options.timeoutMs ?? API_TIMEOUT_MS);

  try {
    const response = await fetch(url, {
      method: endpoint.method,
      headers,
      body: payload,
      signal: controller.signal,
    });

    const headerRequestId = response.headers.get('x-request-id') ?? undefined;
    const text = await response.text();
    const body = parseJson(text);

    if (!response.ok) {
      if (response.status === 401) unauthorizedHandler?.();
      throw apiErrorFromResponse(response.status, body, headerRequestId);
    }

    const parsed = endpoint.response.safeParse(body);
    if (!parsed.success) {
      throw new ApiError({
        status: response.status,
        type: 'invalid-response',
        message: 'The server returned an unexpected response.',
        hint: parsed.error.issues,
        requestId: requestIdValue(headerRequestId),
      });
    }
    return parsed.data;
  } catch (error) {
    if (timedOut.value) {
      throw new ApiError({
        status: 0,
        type: 'timeout',
        message: 'The request timed out. Check the connection and try again.',
      });
    }
    if (options.signal?.aborted === true) throw error;
    if (error instanceof ApiError) throw error;
    throw new ApiError({
      status: 0,
      type: 'network',
      message: 'Could not reach the server. Check your connection.',
    });
  } finally {
    clearTimeout(timer);
    options.signal?.removeEventListener('abort', forwardAbort);
  }
}

/**
 * Keeps the separators of a wildcard path parameter, escaping each segment.
 * `.` and `..` are rejected so a user-controlled value cannot escape the
 * endpoint's path.
 */
function encodeSlashedPathParam(name: string, value: string): string {
  return value
    .split('/')
    .map((segment) => {
      if (segment === '' || segment === '.' || segment === '..') {
        throw new ApiError({
          status: 0,
          type: 'param-malformed',
          message: `Invalid path parameter "${name}".`,
        });
      }
      return encodeURIComponent(segment);
    })
    .join('/');
}

function requestIdValue(value: string | null | undefined): string | undefined {
  if (value === null || value === undefined) return undefined;
  const trimmed = value.trim();
  return trimmed === '' ? undefined : trimmed;
}

function fallbackErrorMessage(status: number): string {
  return status >= 500
    ? `The server is unavailable (HTTP ${String(status)}). Try again.`
    : `Request failed (HTTP ${String(status)}).`;
}

function parseJson(text: string): unknown {
  if (text === '') return undefined;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

interface SerializedBody {
  payload: BodyInit | undefined;
  contentType: string | null;
}

/**
 * JSON is the default. Blobs and form bodies are sent untouched so storage
 * uploads can reuse the same request plumbing.
 */
function serializeBody(body: unknown): SerializedBody {
  if (body === undefined) return { payload: undefined, contentType: null };
  if (body instanceof Blob) {
    return {
      payload: body,
      contentType: body.type === '' ? null : body.type,
    };
  }
  if (
    body instanceof FormData ||
    body instanceof URLSearchParams ||
    body instanceof ArrayBuffer ||
    body instanceof Uint8Array
  ) {
    return { payload: body as BodyInit, contentType: null };
  }
  return { payload: JSON.stringify(body), contentType: 'application/json' };
}
