import { isApiError } from '@/lib/api/client';
import { cliApproveTarget, safeRedirectPath } from './login';

/**
 * Where the Google callback lands. The server returns the path it stored from
 * `redirect_path`; it is still checked here, because a redirect target is never
 * trusted just because it came from a URL (C.1).
 */
export function oauthCallbackTarget(
  ticket: string | undefined,
  redirectPath: string | undefined,
): string {
  if (ticket !== undefined && ticket !== '') return cliApproveTarget(ticket);
  return safeRedirectPath(redirectPath);
}

/** Server text when it is useful; the step's fallback otherwise. */
export function authErrorMessage(error: unknown, fallback: string): string {
  if (isApiError(error) && error.message !== '') return error.message;
  return fallback;
}
