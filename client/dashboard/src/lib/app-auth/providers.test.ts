import { describe, expect, it } from 'vitest';
import type { OAuthClient } from '@/lib/api/endpoints';
import {
  AUTH_PROVIDERS,
  appleClientMeta,
  clerkDiscoveryEndpoint,
  clientsForProvider,
  findClientName,
  firebaseDiscoveryEndpoint,
  firebaseProjectIdFrom,
  parseAudienceList,
  providerLabel,
  providerStatus,
  redirectUri,
} from './providers';

const googleClient: OAuthClient = {
  id: 'client-1',
  provider_id: 'provider-google',
  client_name: 'google-web',
};

describe('auth provider helpers', () => {
  it('suffixes client names until they are unique', () => {
    expect(findClientName('google-web', new Set())).toBe('google-web');
    expect(findClientName('google-web', new Set(['google-web']))).toBe(
      'google-web2',
    );
    expect(
      findClientName('google-web', new Set(['google-web', 'google-web2'])),
    ).toBe('google-web3');
  });

  it('describes configured and empty providers', () => {
    expect(providerStatus(0)).toBe('Not configured');
    expect(providerStatus(1)).toBe('Configured · 1 client');
    expect(providerStatus(2)).toBe('Configured · 2 clients');
  });

  it('builds the callback and the derived provider endpoints', () => {
    expect(redirectUri('https://api.example.com/')).toBe(
      'https://api.example.com/runtime/oauth/callback',
    );
    expect(firebaseDiscoveryEndpoint('my-project')).toBe(
      'https://securetoken.google.com/my-project/.well-known/openid-configuration',
    );
    expect(firebaseProjectIdFrom(firebaseDiscoveryEndpoint('my-project'))).toBe(
      'my-project',
    );
    expect(firebaseProjectIdFrom('https://example.com/discovery')).toBe('');
  });

  it('parses extra audience IDs from a comma or newline separated list', () => {
    expect(parseAudienceList('')).toEqual([]);
    expect(
      parseAudienceList(
        'com.example.ios, com.example.android\ncom.example.ios',
      ),
    ).toEqual(['com.example.ios', 'com.example.android']);
  });

  it('keeps Apple meta only when both identifiers are present', () => {
    expect(appleClientMeta('TEAM', 'KEY')).toEqual({
      teamId: 'TEAM',
      keyId: 'KEY',
    });
    expect(appleClientMeta('TEAM', '')).toBeNull();
    expect(appleClientMeta('', 'KEY')).toBeNull();
  });

  it('builds a Clerk discovery endpoint when the key carries a domain', () => {
    // base64("clerk.example.com$")
    const key = 'pk_test_Y2xlcmsuZXhhbXBsZS5jb20k';
    expect(clerkDiscoveryEndpoint(key)).toBe(
      'https://clerk.example.com/.well-known/openid-configuration',
    );
    expect(clerkDiscoveryEndpoint('not-a-key')).toBeUndefined();
  });

  it('groups clients by provider and labels unknown providers', () => {
    expect(clientsForProvider([googleClient], 'provider-google')).toEqual([
      googleClient,
    ]);
    expect(clientsForProvider([googleClient], undefined)).toEqual([]);
    expect(providerLabel('google')).toBe('Google');
    expect(providerLabel('okta')).toBe('okta');
    expect(providerLabel('')).toBe('Unknown provider');
    expect(AUTH_PROVIDERS).toContain('google');
  });
});
