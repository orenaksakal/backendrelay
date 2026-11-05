import type { AuthorizedOrigin } from '@/lib/api/endpoints';

/**
 * C.11 redirect origins: the four services the server accepts, their display
 * values and the client-side checks that run before the request. The
 * validation rules are ported from `client/www/components/dash/auth/Origins.tsx`
 * so the old dashboard's edge cases keep working.
 */

export const ORIGIN_SERVICES = [
  'generic',
  'vercel',
  'netlify',
  'custom-scheme',
] as const;

export type OriginService = (typeof ORIGIN_SERVICES)[number];

export const ORIGIN_SERVICE_LABELS: Record<OriginService, string> = {
  generic: 'Website',
  vercel: 'Vercel previews',
  netlify: 'Netlify previews',
  'custom-scheme': 'App scheme',
};

export function isOriginService(value: unknown): value is OriginService {
  return (
    typeof value === 'string' &&
    (ORIGIN_SERVICES as readonly string[]).includes(value)
  );
}

export interface OriginValidationOk {
  ok: true;
  params: string[];
}

export interface OriginValidationError {
  ok: false;
  message: string;
}

export type OriginValidation = OriginValidationOk | OriginValidationError;

function genericOrigin(raw: string): OriginValidation {
  const trimmed = raw.trim();
  if (trimmed === '') return { ok: false, message: 'Enter an origin.' };
  try {
    const url = new URL(trimmed);
    const host = url.host;
    if (host === '') throw new Error('missing host');
    // Localhost with a port is a valid development origin; a bare single-label
    // host is not.
    if (host.split('.').length === 1 && url.port === '') {
      throw new Error('invalid url');
    }
    return { ok: true, params: [host] };
  } catch {
    if (!trimmed.startsWith('http')) {
      return genericOrigin(`http://${trimmed}`);
    }
    return { ok: false, message: 'Invalid URL.' };
  }
}

function customScheme(raw: string): OriginValidation {
  const trimmed = raw.trim();
  if (trimmed === '') return { ok: false, message: 'Enter an app scheme.' };
  try {
    const url = new URL(trimmed);
    const scheme = url.protocol.slice(0, -1);
    if (scheme === '') throw new Error('missing scheme');
    return { ok: true, params: [scheme] };
  } catch {
    return { ok: false, message: 'Invalid scheme.' };
  }
}

/** Validates one row of the Add origin dialog and returns the `params` array. */
export function validateOrigin(
  service: OriginService,
  raw: string,
): OriginValidation {
  const value = raw.trim();
  if (value === '') {
    return { ok: false, message: 'Enter an origin or project name.' };
  }
  switch (service) {
    case 'generic':
      return genericOrigin(value);
    case 'netlify':
      return { ok: true, params: [value] };
    case 'vercel':
      return { ok: true, params: ['vercel.app', value] };
    case 'custom-scheme':
      return customScheme(value);
  }
}

export function originDisplay(origin: AuthorizedOrigin): string {
  switch (origin.service) {
    case 'generic':
    case 'netlify':
      return origin.params[0] ?? '';
    case 'vercel':
      return origin.params[1] ?? '';
    case 'custom-scheme':
      return `${origin.params[0] ?? ''}://`;
    default:
      return origin.params[0] ?? '';
  }
}

export function originTypeLabel(service: string): string {
  return isOriginService(service)
    ? ORIGIN_SERVICE_LABELS[service]
    : service === ''
      ? 'Unknown type'
      : service;
}

export function originInputLabel(service: OriginService): string {
  switch (service) {
    case 'generic':
      return 'Origin domain';
    case 'netlify':
      return 'Netlify site';
    case 'vercel':
      return 'Vercel project';
    case 'custom-scheme':
      return 'App scheme';
  }
}

export function originInputPlaceholder(service: OriginService): string {
  switch (service) {
    case 'generic':
      return 'example.com';
    case 'netlify':
      return 'netlify-site-name';
    case 'vercel':
      return 'vercel-project-name';
    case 'custom-scheme':
      return 'app-scheme://';
  }
}

export function originHelp(service: OriginService): string | undefined {
  switch (service) {
    case 'vercel':
      return 'Allows every preview URL for the Vercel project.';
    case 'netlify':
      return 'Allows every preview URL for the Netlify site.';
    case 'custom-scheme':
      return 'Use an app scheme when a native app starts the OAuth flow.';
    case 'generic':
      return undefined;
  }
}
