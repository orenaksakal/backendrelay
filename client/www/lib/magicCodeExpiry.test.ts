import { describe, expect, it } from 'vitest';

import {
  effectiveMagicCodeExpiryMinutes,
  magicCodeExpirationLabel,
} from './magicCodeExpiry';

describe('magic code expiry', () => {
  it('uses the backend 24-hour default when the app override is unset', () => {
    expect(effectiveMagicCodeExpiryMinutes(null)).toBe(1440);
    expect(magicCodeExpirationLabel(null)).toBe('24 hours');
  });

  it('preserves explicit minute and hour overrides', () => {
    expect(magicCodeExpirationLabel(10)).toBe('10 minutes');
    expect(magicCodeExpirationLabel(60)).toBe('1 hour');
  });
});
