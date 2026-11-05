import { z } from 'zod';
import { ApiError, apiErrorFromResponse } from '@/lib/api/client';

/**
 * The `@instantdb/admin` calls the dashboard makes directly: the C.8 auth
 * methods and the C.10 Test-as debug endpoints. The admin SDK is not part of
 * the dashboard bundle, so this module keeps the same request shapes:
 * `POST /admin/...?app_id=<app>`, `app-id` and a bearer app admin token. The
 * refresh token `createToken` returns is deliberately dropped: C.8 shows "User
 * created", never a credential.
 */

/** Matches the dashboard API client's deadline. */
export const ADMIN_TIMEOUT_MS = 30_000;

export interface AppAdminConfig {
  /** The app's API origin, not the dashboard gateway. */
  apiBaseUrl: string;
  appId: string;
  adminToken: string;
}

/** The config for admin auth actions, or `null` when the payload lacks one. */
export function appAdminConfig({
  appId,
  adminToken,
  apiBaseUrl,
}: {
  appId: string;
  adminToken: string | undefined;
  apiBaseUrl: string | undefined;
}): AppAdminConfig | null {
  if (apiBaseUrl === undefined || apiBaseUrl === '') return null;
  if (adminToken === undefined || adminToken === '') return null;
  return { apiBaseUrl, appId, adminToken };
}

/** The admin URL: the app origin plus `app_id`, as the SDK builds it. */
export function appAdminUrl(
  config: AppAdminConfig,
  path: string,
  params: Record<string, string> = {},
): string {
  const url = new URL(`${config.apiBaseUrl.replace(/\/$/, '')}${path}`);
  url.searchParams.set('app_id', config.appId);
  for (const [key, value] of Object.entries(params)) {
    url.searchParams.set(key, value);
  }
  return url.toString();
}

const magicCodeResponseSchema = z.looseObject({ code: z.string() });
const signOutResponseSchema = z.looseObject({ ok: z.boolean().optional() });
const deleteUserResponseSchema = z.looseObject({ deleted: z.unknown() });
const createTokenResponseSchema = z.looseObject({
  user: z.looseObject({ refresh_token: z.string() }),
});

interface AdminRequest {
  method: 'GET' | 'POST' | 'DELETE';
  body?: unknown;
  /** Selector parameters the `/admin/users` lookup reads from the query. */
  params?: Record<string, string>;
  /** Impersonation and debug headers layered over the admin token. */
  headers?: Record<string, string>;
}

function parseJson(text: string): unknown {
  if (text === '') return undefined;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

export async function adminFetch<TResponse extends z.ZodType>(
  config: AppAdminConfig,
  path: string,
  request: AdminRequest,
  schema: TResponse,
): Promise<z.output<TResponse>> {
  const url = appAdminUrl(config, path, request.params);
  const controller = new AbortController();
  const timedOut = { value: false };
  const timer = setTimeout(() => {
    timedOut.value = true;
    controller.abort();
  }, ADMIN_TIMEOUT_MS);

  try {
    const response = await fetch(url, {
      method: request.method,
      headers: {
        accept: 'application/json',
        'content-type': 'application/json',
        'app-id': config.appId,
        authorization: `Bearer ${config.adminToken}`,
        ...request.headers,
      },
      body:
        request.body === undefined ? undefined : JSON.stringify(request.body),
      signal: controller.signal,
    });

    const headerRequestId = response.headers.get('x-request-id') ?? undefined;
    const body = parseJson(await response.text());

    if (!response.ok) {
      throw apiErrorFromResponse(response.status, body, headerRequestId);
    }

    const parsed = schema.safeParse(body);
    if (!parsed.success) {
      throw new ApiError({
        status: response.status,
        type: 'invalid-response',
        message: 'The server returned an unexpected response.',
        hint: parsed.error.issues,
        requestId: headerRequestId,
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
    if (error instanceof ApiError) throw error;
    throw new ApiError({
      status: 0,
      type: 'network',
      message: 'Could not reach the server. Check your connection.',
    });
  } finally {
    clearTimeout(timer);
  }
}

/** `auth.sendMagicCode`; the code travels by email, not to the browser. */
export async function sendMagicCode(
  config: AppAdminConfig,
  email: string,
): Promise<void> {
  await adminFetch(
    config,
    '/admin/send_magic_code',
    { method: 'POST', body: { email } },
    magicCodeResponseSchema,
  );
}

/** `auth.createToken`, which creates the user when the email is new. */
export async function createUser(
  config: AppAdminConfig,
  email: string,
): Promise<void> {
  await adminFetch(
    config,
    '/admin/refresh_tokens',
    { method: 'POST', body: { email } },
    createTokenResponseSchema,
  );
}

/** `auth.signOut`: every refresh token for the user stops working. */
export async function signOutUser(
  config: AppAdminConfig,
  userId: string,
): Promise<void> {
  await adminFetch(
    config,
    '/admin/sign_out',
    { method: 'POST', body: { id: userId } },
    signOutResponseSchema,
  );
}

/** `auth.deleteUser`; the user's own data is left untouched. */
export async function deleteUser(
  config: AppAdminConfig,
  userId: string,
): Promise<void> {
  await adminFetch(
    config,
    '/admin/users',
    { method: 'DELETE', params: { id: userId } },
    deleteUserResponseSchema,
  );
}
