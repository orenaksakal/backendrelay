import type { OauthApp } from '@/lib/api/endpoints';
import type { EffectiveRole } from '@/lib/api/endpoints/schemas';
import { roleRank } from '@/lib/organization/model';

/**
 * C.14 OAuth app model: the role gate, the logo limits (B.3-style upload), the
 * app-info draft used by the UnsavedChangesBar and the redirect URL checks that
 * mirror the server's `validate_redirects`.
 */

/** The dashboard accepts PNG and SVG logos up to 256 KB (C.14). */
export const OAUTH_APP_LOGO_MAX_BYTES = 256 * 1024;
export const OAUTH_APP_LOGO_ACCEPT = 'image/png,image/svg+xml';

const LOGO_TYPES: ReadonlySet<string> = new Set(['image/png', 'image/svg+xml']);
const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

/** Reading and publishing OAuth apps needs the collaborator role (server check). */
export function canViewOAuthApps(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('collaborator');
}

/** Creating apps, clients and secrets, and editing the app info. */
export function canManageOAuthApps(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('collaborator');
}

/** Deleting an OAuth app or one of its clients needs the admin role. */
export function canDeleteOAuthApps(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('admin');
}

export function clientCount(app: Pick<OauthApp, 'clients'>): number {
  return app.clients?.length ?? 0;
}

export interface OauthAppInfo {
  name: string;
  /** A data URL, or an empty string when the app has no logo. */
  logo: string;
  homePage: string;
  privacyPolicy: string;
  terms: string;
  supportEmail: string;
}

export type OauthAppInfoField = keyof OauthAppInfo;

/** The create form starts from every field empty. */
export const EMPTY_OAUTH_APP_INFO: OauthAppInfo = {
  name: '',
  logo: '',
  homePage: '',
  privacyPolicy: '',
  terms: '',
  supportEmail: '',
};

export type OauthAppInfoProblems = Partial<Record<OauthAppInfoField, string>>;

/** `POST /dash/apps/:appId/oauth-apps/:oauthAppId`; null means "keep". */
export interface OauthAppUpdateBody {
  [key: string]: unknown;
  app_name?: string;
  app_logo?: string | null;
  app_home_page?: string | null;
  app_privacy_policy_link?: string | null;
  app_tos_link?: string | null;
  support_email?: string | null;
}

type OauthAppInfoSource = Pick<
  OauthApp,
  | 'appName'
  | 'appLogo'
  | 'appHomePage'
  | 'appPrivacyPolicyLink'
  | 'appTosLink'
  | 'supportEmail'
>;

export function oauthAppInfo(app: OauthAppInfoSource): OauthAppInfo {
  return {
    name: app.appName,
    logo: app.appLogo ?? '',
    homePage: app.appHomePage ?? '',
    privacyPolicy: app.appPrivacyPolicyLink ?? '',
    terms: app.appTosLink ?? '',
    supportEmail: app.supportEmail ?? '',
  };
}

export function oauthAppInfoEqual(a: OauthAppInfo, b: OauthAppInfo): boolean {
  return (
    a.name === b.name &&
    a.logo === b.logo &&
    a.homePage === b.homePage &&
    a.privacyPolicy === b.privacyPolicy &&
    a.terms === b.terms &&
    a.supportEmail === b.supportEmail
  );
}

/**
 * Only the fields that changed: the server treats a missing or null field as
 * "keep the current value", so a cleared input cannot remove one.
 */
export function oauthAppInfoPatch(
  info: OauthAppInfo,
  current: OauthAppInfo,
): OauthAppUpdateBody {
  const patch: OauthAppUpdateBody = {};
  const name = info.name.trim();
  if (name !== current.name) patch.app_name = name;
  if (info.logo !== current.logo) {
    patch.app_logo = info.logo === '' ? null : info.logo;
  }
  const homePage = info.homePage.trim();
  if (homePage !== current.homePage) {
    patch.app_home_page = homePage === '' ? null : homePage;
  }
  const privacyPolicy = info.privacyPolicy.trim();
  if (privacyPolicy !== current.privacyPolicy) {
    patch.app_privacy_policy_link = privacyPolicy === '' ? null : privacyPolicy;
  }
  const terms = info.terms.trim();
  if (terms !== current.terms) {
    patch.app_tos_link = terms === '' ? null : terms;
  }
  const supportEmail = info.supportEmail.trim();
  if (supportEmail !== current.supportEmail) {
    patch.support_email = supportEmail === '' ? null : supportEmail;
  }
  return patch;
}

/** Field errors shown after the first save attempt. Empty URL/email fields pass. */
export function oauthAppInfoProblems(info: OauthAppInfo): OauthAppInfoProblems {
  const problems: OauthAppInfoProblems = {};
  if (info.name.trim() === '') {
    problems.name = 'Enter a name.';
  }
  const homePage = webUrlProblem(info.homePage);
  if (homePage !== undefined) problems.homePage = homePage;
  const privacy = webUrlProblem(info.privacyPolicy);
  if (privacy !== undefined) problems.privacyPolicy = privacy;
  const terms = webUrlProblem(info.terms);
  if (terms !== undefined) problems.terms = terms;
  const email = info.supportEmail.trim();
  if (email !== '' && !EMAIL_PATTERN.test(email)) {
    problems.supportEmail = 'Enter a valid email address.';
  }
  return problems;
}

/** Absolute http(s) URL, or undefined when the field may stay empty. */
export function webUrlProblem(value: string): string | undefined {
  const trimmed = value.trim();
  if (trimmed === '') return undefined;
  try {
    const url = new URL(trimmed);
    if (
      (url.protocol === 'http:' || url.protocol === 'https:') &&
      url.hostname !== ''
    ) {
      return undefined;
    }
  } catch {
    /* Fall through to the message. */
  }
  return 'Enter an absolute http(s) URL.';
}

export function logoProblem(file: {
  type: string;
  size: number;
}): string | undefined {
  if (!LOGO_TYPES.has(file.type)) {
    return 'Upload a PNG or SVG file.';
  }
  if (file.size > OAUTH_APP_LOGO_MAX_BYTES) {
    return 'The logo must be 256 KB or smaller.';
  }
  return undefined;
}

/** Reads an upload into the data URL the server stores. */
export function readFileAsDataUrl(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      resolve(typeof reader.result === 'string' ? reader.result : '');
    };
    reader.onerror = () => {
      reject(reader.error ?? new Error('Could not read the file.'));
    };
    reader.readAsDataURL(file);
  });
}

export function clientNameProblem(name: string): string | undefined {
  return name.trim() === '' ? 'Enter a client name.' : undefined;
}

/**
 * Mirrors the server's redirect checks. Test-mode apps may use localhost over
 * http; public apps need https. Credentials, fragments and path traversal are
 * never accepted.
 */
export function redirectUrlProblem(
  value: string,
  testMode: boolean,
): string | undefined {
  const trimmed = value.trim();
  if (trimmed === '') return 'Enter a redirect URL.';

  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return 'Enter an absolute URL, such as https://example.com/callback.';
  }

  if (url.username !== '' || url.password !== '') {
    return 'Remove the username and password from the URL.';
  }
  if (url.hash !== '') {
    return 'Remove the #fragment from the redirect URL.';
  }
  if (url.pathname.includes('/..') || url.pathname.includes('\\.')) {
    return 'Remove the path traversal from the URL.';
  }

  const localhost =
    url.hostname === 'localhost' || url.hostname === '127.0.0.1';
  if (localhost) {
    if (!testMode) {
      return 'Localhost redirects only work while the OAuth app is in test mode.';
    }
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      return 'Use http or https for a localhost redirect.';
    }
    return undefined;
  }
  if (url.protocol !== 'https:') {
    return 'Use https for a public redirect URL.';
  }
  return undefined;
}

/** Trims the rows and drops the empty ones; redirect URLs are optional. */
export function cleanRedirectUrls(urls: readonly string[]): string[] {
  return urls.map((url) => url.trim()).filter((url) => url !== '');
}

/** The first invalid or duplicated redirect URL, if any. */
export function redirectUrlsProblem(
  urls: readonly string[],
  testMode: boolean,
): string | undefined {
  const seen = new Set<string>();
  for (const url of cleanRedirectUrls(urls)) {
    const problem = redirectUrlProblem(url, testMode);
    if (problem !== undefined) return problem;
    if (seen.has(url)) return 'This redirect URL is already listed.';
    seen.add(url);
  }
  return undefined;
}
