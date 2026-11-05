import { buildApiUrl, isApiError } from '@/lib/api/client';
import type { RuntimeConfig } from '@/lib/runtime-config';

/** The magic code is six digits and the field submits once it is complete. */
export const MAGIC_CODE_LENGTH = 6;

/** "Resend code" stays disabled for this long (C.1). */
export const RESEND_COOLDOWN_SECONDS = 30;

/** `/login` search params: where to go next and a pending CLI login ticket. */
export interface SignInSearch {
  redirect?: string;
  ticket?: string;
}

/** Send-code failures with a wording the plan dictates (C.1). */
const SEND_CODE_MESSAGES: Record<string, string> = {
  'signups-closed':
    'Sign-ups are closed on this deployment. Ask the operator for access.',
  'rate-limited': 'Too many attempts. Try again in a few minutes.',
};

/** Verify-code failures with a wording the plan dictates (C.1). */
const VERIFY_CODE_MESSAGES: Record<string, string> = {
  'record-expired': 'That code expired. Send a new one.',
  'record-not-found': "That code isn't valid.",
  'param-missing': "That code isn't valid.",
  'param-malformed': "That code isn't valid.",
  'rate-limited': 'Too many attempts. Try again in a few minutes.',
};

const FALLBACK_SEND_MESSAGE = "Couldn't send the code. Try again.";
const FALLBACK_VERIFY_MESSAGE = "Couldn't verify the code. Try again.";

/**
 * Keeps post-sign-in navigation on this origin. A protocol-relative or absolute
 * target would let a crafted `?redirect=` link take the session elsewhere.
 */
export function safeRedirectPath(redirect: string | undefined): string {
  if (redirect === undefined || redirect === '') return '/';
  if (!redirect.startsWith('/') || redirect.startsWith('//')) return '/';
  if (redirect.includes('\\')) return '/';
  return redirect;
}

/** The CLI approval screen for a pending ticket (C.1). */
export function cliApproveTarget(ticket: string): string {
  return `/cli/approve?ticket=${encodeURIComponent(ticket)}`;
}

/** Where a successful sign-in lands: CLI approval first, then the redirect. */
export function postSignInTarget(search: SignInSearch): string {
  if (search.ticket !== undefined && search.ticket !== '') {
    return cliApproveTarget(search.ticket);
  }
  return safeRedirectPath(search.redirect);
}

/** The Google button target: `/dash/oauth/start` on the management origin. */
export function oauthStartUrl(
  config: RuntimeConfig,
  search: SignInSearch,
): string {
  const params: Record<string, string> = {
    redirect_path: safeRedirectPath(search.redirect),
  };
  if (search.ticket !== undefined && search.ticket !== '') {
    params['ticket'] = search.ticket;
  }
  return buildApiUrl('/dash/oauth/start', params, config);
}

/** Strips separators and non-digits so a pasted code becomes six digits. */
export function normalizeMagicCode(value: string): string {
  return value.replace(/\D/g, '').slice(0, MAGIC_CODE_LENGTH);
}

export function sendCodeErrorMessage(error: unknown): string {
  const type = apiErrorType(error);
  if (type !== undefined) {
    const mapped = SEND_CODE_MESSAGES[type];
    if (mapped !== undefined) return mapped;
    if (type === 'validation-failed') {
      const hint = firstHintError(error);
      if (hint !== undefined) return hint;
    }
  }
  return serverMessage(error, FALLBACK_SEND_MESSAGE);
}

export function verifyCodeErrorMessage(error: unknown): string {
  const type = apiErrorType(error);
  const mapped = type === undefined ? undefined : VERIFY_CODE_MESSAGES[type];
  if (mapped !== undefined) return mapped;
  return serverMessage(error, FALLBACK_VERIFY_MESSAGE);
}

function apiErrorType(error: unknown): string | undefined {
  return isApiError(error) ? error.type : undefined;
}

/** `validation-failed` carries field errors in `hint.errors[].message`. */
function firstHintError(error: unknown): string | undefined {
  if (!isApiError(error)) return undefined;
  const hint = error.hint;
  if (typeof hint !== 'object' || hint === null) return undefined;
  const errors = (hint as { errors?: unknown }).errors;
  if (!Array.isArray(errors) || errors.length === 0) return undefined;
  const first = errors[0] as unknown;
  if (typeof first !== 'object' || first === null) return undefined;
  const message = (first as { message?: unknown }).message;
  return typeof message === 'string' && message !== '' ? message : undefined;
}

function serverMessage(error: unknown, fallback: string): string {
  if (!isApiError(error)) return fallback;
  return error.message === '' ? fallback : error.message;
}
