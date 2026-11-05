import { describe, expect, it } from 'vitest';

import { appleClientMeta } from './appleOAuth';

describe('Apple OAuth client metadata', () => {
  it('retains a complete web credential pair', () => {
    expect(appleClientMeta('TEAM', 'KEY')).toEqual({
      teamId: 'TEAM',
      keyId: 'KEY',
    });
  });

  it('uses null to explicitly clear removed web credentials', () => {
    expect(appleClientMeta('', '')).toBeNull();
  });
});
