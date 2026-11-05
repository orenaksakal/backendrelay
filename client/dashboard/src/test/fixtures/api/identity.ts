import { mockSignInToken, mockSignInUser } from './auth';

/**
 * The mocked Google callback and platform consent flows (C.1, C.2).
 * `POST /dash/oauth/token` accepts `mockOAuthCode`; `POST /platform/oauth/claim`
 * answers with `mockConsent`.
 */
export const mockOAuthCode = '22222222-2222-4222-8222-222222222222';

export const mockOAuthRedirectPath = '/apps/app-1/overview';

export const mockOAuthToken = mockSignInToken;

export const mockCliTicket = '11111111-1111-4111-8111-111111111111';

export const mockConsentRedirectId = '33333333-3333-4333-8333-333333333333';

export const mockConsent = {
  appName: 'Linear sync',
  appLogo: null,
  scopes: ['apps-read', 'data-read'],
  redirectOrigin: 'https://client.test',
  userEmail: mockSignInUser.email,
  grantToken: 'grant-token-1',
};
