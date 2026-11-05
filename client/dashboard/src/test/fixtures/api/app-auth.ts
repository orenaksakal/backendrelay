/**
 * C.11 fixtures: two configured providers (Google with two clients, GitHub with
 * one) plus two redirect origin shapes, so every card and table branch renders.
 */
export const mockOAuthProviderGoogle = {
  id: 'provider-google',
  provider_name: 'google',
  created_at: '2026-01-01T00:00:00Z',
};

export const mockOAuthProviderGithub = {
  id: 'provider-github',
  provider_name: 'github',
  created_at: '2026-01-02T00:00:00Z',
};

export const mockGoogleWebClient = {
  id: 'client-google-web',
  provider_id: mockOAuthProviderGoogle.id,
  client_name: 'google-web',
  client_id: 'google-client-id.apps.googleusercontent.com',
  extra_audiences: [],
  discovery_endpoint:
    'https://accounts.google.com/.well-known/openid-configuration',
  redirect_to: null,
  meta: { appType: 'web', skipNonceChecks: true },
  use_shared_credentials: false,
  created_at: 1_761_000_000_000,
};

export const mockGoogleIosClient = {
  id: 'client-google-ios',
  provider_id: mockOAuthProviderGoogle.id,
  client_name: 'google-ios',
  client_id: 'google-ios-client-id.apps.googleusercontent.com',
  extra_audiences: [],
  discovery_endpoint:
    'https://accounts.google.com/.well-known/openid-configuration',
  redirect_to: null,
  meta: { appType: 'ios', skipNonceChecks: true },
  use_shared_credentials: false,
  created_at: 1_761_100_000_000,
};

export const mockGithubClient = {
  id: 'client-github-web',
  provider_id: mockOAuthProviderGithub.id,
  client_name: 'github-web',
  client_id: 'github-client-id',
  extra_audiences: [],
  discovery_endpoint: null,
  redirect_to: null,
  meta: {},
  use_shared_credentials: false,
  created_at: 1_761_200_000_000,
};

export const mockRedirectOrigins = [
  {
    id: 'origin-website',
    service: 'generic',
    params: ['example.com'],
    created_at: '2026-01-03T00:00:00Z',
  },
  {
    id: 'origin-vercel',
    service: 'vercel',
    params: ['vercel.app', 'todo-preview'],
    created_at: '2026-01-04T00:00:00Z',
  },
];

export const mockAppAuth = {
  authorized_redirect_origins: mockRedirectOrigins,
  oauth_service_providers: [mockOAuthProviderGoogle, mockOAuthProviderGithub],
  oauth_clients: [mockGoogleWebClient, mockGoogleIosClient, mockGithubClient],
  default_sender_email: 'verify@example.com',
};

/** C.11 test users: two fixed codes so the reveal and delete states render. */
export const mockTestUsers = [
  {
    id: 'test-user-1',
    app_id: 'app-1',
    email: 'qa@example.com',
    code: '424242',
    created_at: '2026-01-08T00:00:00Z',
  },
  {
    id: 'test-user-2',
    app_id: 'app-1',
    email: 'review@example.com',
    code: '123456',
    created_at: '2026-01-09T00:00:00Z',
  },
];
