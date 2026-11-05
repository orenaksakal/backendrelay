import { describe, expect, test } from 'vitest';

import { PlatformApi } from '../../src/api.ts';
import { OAuthHandler } from '../../src/oauth.ts';

describe('platform backend URL configuration', () => {
  test('PlatformApi requires an apiURI instead of defaulting to instantdb.com', () => {
    expect(() => new PlatformApi({ auth: { token: 'token' } })).toThrow(
      /apiURI/,
    );
  });

  test('OAuthHandler requires an apiURI instead of defaulting to instantdb.com', () => {
    expect(
      () =>
        new OAuthHandler({
          clientId: 'client',
          redirectUri: 'https://app.example.test/oauth',
        } as never),
    ).toThrow(/apiURI/);
  });
});
