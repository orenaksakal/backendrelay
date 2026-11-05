import { describe, expect, it } from 'vitest';
import { accountTimestamp } from './dates';

describe('account timestamps', () => {
  it('treats timezone-less server values as UTC', () => {
    expect(accountTimestamp('2026-01-02T10:00:00')).toBe(
      '2026-01-02T10:00:00Z',
    );
    expect(accountTimestamp('2026-01-02T10:00:00.123')).toBe(
      '2026-01-02T10:00:00.123Z',
    );
  });

  it('keeps explicit offsets and epoch values unchanged', () => {
    expect(accountTimestamp('2026-01-02T10:00:00Z')).toBe(
      '2026-01-02T10:00:00Z',
    );
    expect(accountTimestamp('2026-01-02T10:00:00+02:00')).toBe(
      '2026-01-02T10:00:00+02:00',
    );
    expect(accountTimestamp(1767348000000)).toBe(1767348000000);
  });
});
