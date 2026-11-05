export type { Request } from '@/lib/request';
export { requestError } from '@/lib/request';
export type AccountTab = 'token' | 'oauth' | 'invites';
export type AccessToken = {
  id: string;
  name: string;
  created_at: string | number;
};
export type ConnectedApp = {
  id: string;
  name: string;
  logo: string | null;
  homePage: string | null;
  privacyPolicyLink: string | null;
  tosLink: string | null;
};
export type Invitation = {
  id: string;
  type: 'app' | 'org';
  foreign_key: string;
  title: string;
  invitee_role: string;
  inviter_email: string;
};
export const accountTabs: {
  id: AccountTab;
  label: string;
  description: string;
}[] = [
  {
    id: 'token',
    label: 'Access tokens',
    description: 'Credentials for the Platform API',
  },
  {
    id: 'oauth',
    label: 'Connected applications',
    description: 'Integrations authorized by your account',
  },
  {
    id: 'invites',
    label: 'Invitations',
    description: 'Application and organization access',
  },
];
export function tokenName(value: string) {
  const name = value.trim();
  if (!name) throw new Error('Enter a token name.');
  return name;
}
export function tokenMetadata(value: unknown): AccessToken {
  if (
    !value ||
    typeof value !== 'object' ||
    !('id' in value) ||
    typeof value.id !== 'string' ||
    !('name' in value) ||
    typeof value.name !== 'string' ||
    !('created_at' in value) ||
    (typeof value.created_at !== 'string' &&
      typeof value.created_at !== 'number')
  )
    throw new Error(
      'The token response is incomplete. Refresh the inventory before trying again.',
    );
  return { id: value.id, name: value.name, created_at: value.created_at };
}
export function tokenDate(value: string | number): string | null {
  const date = new Date(
    typeof value === 'string' &&
    /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?$/.test(value)
      ? value + 'Z'
      : value,
  );
  return Number.isFinite(date.getTime()) ? date.toISOString() : null;
}
export function filterTokens(
  tokens: AccessToken[],
  search: string,
  sort: 'newest' | 'name',
) {
  return tokens
    .filter((t) =>
      [t.name, t.id].some((v) =>
        v.toLowerCase().includes(search.trim().toLowerCase()),
      ),
    )
    .sort((a, b) =>
      sort === 'name'
        ? a.name.localeCompare(b.name)
        : (Date.parse(tokenDate(b.created_at) ?? '') || 0) -
            (Date.parse(tokenDate(a.created_at) ?? '') || 0) ||
          a.name.localeCompare(b.name),
    );
}
export function safeAccountUrl(value: string | null) {
  try {
    if (!value) return null;
    const u = new URL(value);
    return ['https:', 'http:'].includes(u.protocol) &&
      !u.username &&
      !u.password
      ? u.href
      : null;
  } catch {
    return null;
  }
}
export function filterConnected(apps: ConnectedApp[], search: string) {
  return apps
    .filter((a) =>
      [a.id, a.name, a.homePage].some((v) =>
        v?.toLowerCase().includes(search.trim().toLowerCase()),
      ),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
}
export function filterInvitations(
  invites: Invitation[],
  search: string,
  type: string,
) {
  return invites.filter(
    (i) =>
      (type === 'all' || i.type === type) &&
      [i.title, i.inviter_email, i.invitee_role, i.id].some((v) =>
        v.toLowerCase().includes(search.trim().toLowerCase()),
      ),
  );
}
export type AccountAction =
  | { kind: 'create'; name: string }
  | { kind: 'revoke-token'; token: AccessToken }
  | { kind: 'revoke-app'; app: ConnectedApp }
  | { kind: 'accept' | 'decline'; invite: Invitation };
export function accountRequest(action: AccountAction) {
  switch (action.kind) {
    case 'create':
      return {
        path: '/dash/personal_access_tokens',
        init: {
          method: 'POST',
          body: JSON.stringify({ name: tokenName(action.name) }),
        },
      };
    case 'revoke-token':
      return {
        path: `/dash/personal_access_tokens/${encodeURIComponent(action.token.id)}`,
        init: { method: 'DELETE' },
      };
    case 'revoke-app':
      return {
        path: '/dash/user/oauth_apps/revoke_access',
        init: {
          method: 'POST',
          body: JSON.stringify({ oauthAppId: action.app.id }),
        },
      };
    default:
      return {
        path: `/dash/invites/${action.kind}`,
        init: {
          method: 'POST',
          body: JSON.stringify({ 'invite-id': action.invite.id }),
        },
      };
  }
}
export function inviteDestination(invite: Invitation) {
  return invite.type === 'org'
    ? `/dash/org?org=${encodeURIComponent(invite.foreign_key)}`
    : `/dash?${new URLSearchParams({ s: 'main', t: 'home', app: invite.foreign_key })}`;
}
export function connectedApps(value: unknown): ConnectedApp[] {
  if (!Array.isArray(value))
    throw new Error('Connected application inventory is invalid.');
  return value.map((item) => {
    if (!item || typeof item.id !== 'string' || typeof item.name !== 'string')
      throw new Error('Connected application metadata is invalid.');
    const optional = (key: string) =>
      typeof item[key] === 'string' ? item[key] : null;
    return {
      id: item.id,
      name: item.name,
      logo: optional('logo'),
      homePage: optional('homePage'),
      privacyPolicyLink: optional('privacyPolicyLink'),
      tosLink: optional('tosLink'),
    };
  });
}
export function accountInvitations(value: unknown): Invitation[] {
  if (value === undefined) return [];
  if (!Array.isArray(value))
    throw new Error('Invitation inventory is invalid.');
  return value.map((item) => {
    if (
      !item ||
      !['app', 'org'].includes(item.type) ||
      ['id', 'foreign_key', 'title', 'invitee_role', 'inviter_email'].some(
        (key) => typeof item[key] !== 'string',
      )
    )
      throw new Error('Invitation metadata is invalid.');
    return {
      id: item.id,
      type: item.type,
      foreign_key: item.foreign_key,
      title: item.title,
      invitee_role: item.invitee_role,
      inviter_email: item.inviter_email,
    };
  });
}
