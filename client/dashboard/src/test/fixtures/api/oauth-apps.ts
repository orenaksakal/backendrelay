import type {
  OauthApp,
  OauthAppClient,
  OauthAppClientSecret,
} from '@/lib/api/endpoints';

/**
 * OAuth app fixtures (C.14). The request and response shapes mirror
 * `dashboard_control.rs` so the screens can be exercised without a server.
 */
export const mockOauthClientSecret: OauthAppClientSecret = {
  id: 'oauth-secret-1',
  clientId: 'oauth-client-1',
  firstFour: 'a1b2',
  createdAt: '2026-01-11T10:00:00Z',
};

export const mockOauthClient: OauthAppClient = {
  clientId: 'oauth-client-1',
  oauthAppId: 'oauth-app-1',
  clientName: 'Acme CLI',
  authorizedRedirectUrls: [
    'https://acme.test/callback',
    'http://localhost:3000/callback',
  ],
  clientSecrets: [mockOauthClientSecret],
  createdAt: '2026-01-11T10:00:00Z',
  updatedAt: '2026-01-11T10:00:00Z',
};

export const mockOauthApp: OauthApp = {
  id: 'oauth-app-1',
  appId: 'app-1',
  appName: 'Acme Tools',
  appLogo: null,
  grantedScopes: [],
  isPublic: false,
  supportEmail: 'support@acme.test',
  appHomePage: 'https://acme.test',
  appPrivacyPolicyLink: 'https://acme.test/privacy',
  appTosLink: 'https://acme.test/terms',
  createdAt: '2026-01-10T10:00:00Z',
  updatedAt: '2026-01-12T10:00:00Z',
  clients: [mockOauthClient],
};

export const mockOauthApps: OauthApp[] = [
  mockOauthApp,
  {
    id: 'oauth-app-2',
    appId: 'app-1',
    appName: 'Docs portal',
    appLogo: null,
    grantedScopes: [],
    isPublic: false,
    supportEmail: null,
    appHomePage: null,
    appPrivacyPolicyLink: null,
    appTosLink: null,
    createdAt: '2026-02-10T10:00:00Z',
    updatedAt: '2026-02-10T10:00:00Z',
    clients: [],
  },
];

export const mockCreatedOauthClientSecret: OauthAppClientSecret = {
  id: 'oauth-secret-2',
  clientId: 'oauth-client-1',
  firstFour: '9z8y',
  createdAt: '2026-03-01T10:00:00Z',
};

export const mockOauthClientSecretValue =
  'oauth-client-secret-value-0123456789';
