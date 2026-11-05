/**
 * Account fixtures (C.18). They mirror the endpoints the old dashboard uses so
 * the screens can be exercised without a server.
 */
export const mockPersonalTokens = [
  {
    id: 'token-1',
    user_id: 'user-1',
    name: 'Platform Token',
    created_at: '2026-01-02T10:00:00Z',
    last_four: '9c1d',
  },
];

export const mockCreatedPersonalToken = {
  id: 'token-2',
  user_id: 'user-1',
  name: 'CI deploy',
  created_at: '2026-02-01T10:00:00Z',
  last_four: '4e2f',
  token: 'per_1234567890abcd4e2f',
};

export const mockAuthorizedApps = [
  {
    id: 'oauth-1',
    name: 'Linear sync',
    logo: null,
    homePage: 'https://linear.app',
    privacyPolicyLink: 'https://linear.app/privacy',
    tosLink: 'https://linear.app/terms',
    scopes: ['apps-read', 'data-read'],
    authorizedAt: '2026-01-15T09:00:00Z',
  },
  {
    id: 'oauth-2',
    name: 'Release notes',
    logo: null,
    homePage: null,
    privacyPolicyLink: null,
    tosLink: null,
    scopes: ['apps-read'],
    authorizedAt: '2026-01-20T09:00:00Z',
  },
];

export const mockPendingInvites = [
  {
    id: 'invite-1',
    type: 'org' as const,
    foreign_key: 'org-1',
    title: 'Acme',
    invitee_role: 'collaborator' as const,
    inviter_email: 'admin@example.com',
  },
  {
    id: 'invite-2',
    type: 'app' as const,
    foreign_key: 'app-1',
    title: 'todo-app',
    invitee_role: 'admin' as const,
    inviter_email: 'dev@example.com',
  },
];
