import {
  clerkDomainFromPublishableKey,
  oauthCallbackURL,
} from '@instantdb/platform';
import {
  Briefcase,
  Flame,
  GitBranch,
  Globe,
  KeyRound,
  Mail,
  type LucideIcon,
} from 'lucide-react';
import type { OAuthClient, OAuthProvider } from '@/lib/api/endpoints';

/**
 * C.11 providers tab: the provider catalog, client naming and the small
 * provider-specific derivations the add/edit forms share. Values mirror the
 * old dashboard's request shapes so the server sees exactly what it expects.
 */

export const AUTH_PROVIDERS = [
  'google',
  'apple',
  'github',
  'linkedin',
  'clerk',
  'firebase',
] as const;

export type AuthProvider = (typeof AUTH_PROVIDERS)[number];

export interface AuthProviderDefinition {
  id: AuthProvider;
  label: string;
  /** One-line description under the card title. */
  description: string;
  /** Prefix for the suggested client name. */
  clientPrefix: string;
  icon: LucideIcon;
}

export const AUTH_PROVIDER_DEFINITIONS: Record<
  AuthProvider,
  AuthProviderDefinition
> = {
  google: {
    id: 'google',
    label: 'Google',
    description: 'Google accounts, including native app sign-in.',
    clientPrefix: 'google-web',
    icon: Globe,
  },
  apple: {
    id: 'apple',
    label: 'Apple',
    description: 'Sign in with Apple, including the web redirect flow.',
    clientPrefix: 'apple',
    icon: KeyRound,
  },
  github: {
    id: 'github',
    label: 'GitHub',
    description: 'GitHub OAuth apps for developer tools.',
    clientPrefix: 'github-web',
    icon: GitBranch,
  },
  linkedin: {
    id: 'linkedin',
    label: 'LinkedIn',
    description: 'LinkedIn OpenID Connect.',
    clientPrefix: 'linkedin-web',
    icon: Briefcase,
  },
  clerk: {
    id: 'clerk',
    label: 'Clerk',
    description: 'Accept Clerk session tokens as sign-in.',
    clientPrefix: 'clerk',
    icon: KeyRound,
  },
  firebase: {
    id: 'firebase',
    label: 'Firebase',
    description: 'Accept Firebase Auth ID tokens.',
    clientPrefix: 'firebase',
    icon: Flame,
  },
};

/** The Guest / magic-code card has no OAuth client to configure. */
export const MAGIC_CODE_PROVIDER = {
  label: 'Guest / magic code',
  description: 'Email one-time codes are always available.',
  icon: Mail,
};

export const GOOGLE_APP_TYPES = [
  'web',
  'ios',
  'android',
  'button-for-web',
] as const;

export type GoogleAppType = (typeof GOOGLE_APP_TYPES)[number];

export const GOOGLE_APP_TYPE_LABELS: Record<GoogleAppType, string> = {
  web: 'Web',
  ios: 'iOS',
  android: 'Android',
  'button-for-web': 'Button for web',
};

export function firebaseDiscoveryEndpoint(projectId: string): string {
  return `https://securetoken.google.com/${projectId.trim()}/.well-known/openid-configuration`;
}

export function firebaseProjectIdFrom(discovery: string | undefined): string {
  return (
    discovery?.match(
      /^https:\/\/securetoken\.google\.com\/(.+)\/\.well-known\/openid-configuration$/,
    )?.[1] ?? ''
  );
}

export function clerkDiscoveryEndpoint(
  publishableKey: string,
): string | undefined {
  const domain = clerkDomainFromPublishableKey(publishableKey);
  if (domain === null || !/^[a-z0-9.-]+\.[a-z]{2,}$/i.test(domain)) {
    return undefined;
  }
  return `https://${domain}/.well-known/openid-configuration`;
}

export function clerkDomain(publishableKey: string): string | undefined {
  return clerkDomainFromPublishableKey(publishableKey) ?? undefined;
}

/** Meta values survive the API schema as `unknown`; read them defensively. */
export function metaString(meta: unknown, key: string): string {
  if (typeof meta !== 'object' || meta === null) return '';
  const value = (meta as Record<string, unknown>)[key];
  return typeof value === 'string' ? value : '';
}

export function metaBoolean(meta: unknown, key: string): boolean {
  if (typeof meta !== 'object' || meta === null) return false;
  return (meta as Record<string, unknown>)[key] === true;
}

export function googleAppTypeFrom(meta: unknown): GoogleAppType {
  const value = metaString(meta, 'appType');
  return GOOGLE_APP_TYPES.find((candidate) => candidate === value) ?? 'web';
}

/** Extra audiences are typed as a comma or newline separated list. */
export function parseAudienceList(value: string): string[] {
  const audiences: string[] = [];
  for (const part of value.split(/[\n,]+/)) {
    const audience = part.trim();
    if (audience !== '' && !audiences.includes(audience)) {
      audiences.push(audience);
    }
  }
  return audiences;
}

export function appleClientMeta(teamId: string, keyId: string) {
  return teamId && keyId ? { teamId, keyId } : null;
}

/** `google-web`, then `google-web2`, … so client names stay unique per app. */
export function findClientName(
  prefix: string,
  used: ReadonlySet<string>,
): string {
  if (!used.has(prefix)) return prefix;
  for (let index = 2; ; index += 1) {
    const candidate = `${prefix}${String(index)}`;
    if (!used.has(candidate)) return candidate;
  }
}

export function usedClientNames(
  clients: ReadonlyArray<OAuthClient>,
): Set<string> {
  return new Set(clients.map((client) => client.client_name));
}

export function clientsForProvider(
  clients: ReadonlyArray<OAuthClient>,
  providerId: string | undefined,
): OAuthClient[] {
  if (providerId === undefined) return [];
  return clients.filter((client) => client.provider_id === providerId);
}

export function providersByName(
  providers: ReadonlyArray<OAuthProvider>,
): Record<string, OAuthProvider> {
  return Object.fromEntries(
    providers.map((provider) => [provider.provider_name, provider] as const),
  );
}

export function providersById(
  providers: ReadonlyArray<OAuthProvider>,
): Record<string, OAuthProvider> {
  return Object.fromEntries(
    providers.map((provider) => [provider.id, provider] as const),
  );
}

/** "Configured · 2 clients" or "Not configured" (C.11). */
export function providerStatus(clientCount: number): string {
  if (clientCount === 0) return 'Not configured';
  return `Configured · ${String(clientCount)} ${clientCount === 1 ? 'client' : 'clients'}`;
}

export function providerLabel(name: string): string {
  const provider = AUTH_PROVIDERS.find((candidate) => candidate === name);
  if (provider !== undefined) return AUTH_PROVIDER_DEFINITIONS[provider].label;
  return name === '' ? 'Unknown provider' : name;
}

/** The URI to register with every provider, from the runtime API base. */
export function redirectUri(apiBaseUrl: string | undefined): string {
  return oauthCallbackURL(apiBaseUrl ?? '');
}
