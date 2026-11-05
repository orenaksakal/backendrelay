import { describe, expect, it } from 'vitest';

import { displayAppStatus } from './appStatus';

describe('dashboard app status', () => {
  it('prefers the backend effective status', () => {
    expect(
      displayAppStatus({ status: 'active', effective_status: 'read-only' }),
    ).toBe('read-only');
  });

  it('uses an explicit app status when the backend has no effective status', () => {
    expect(displayAppStatus({ status: 'disabled' })).toBe('disabled');
    expect(displayAppStatus({ status: 'read-only' })).toBe('read-only');
  });

  it('defaults apps with no status to active', () => {
    expect(displayAppStatus({})).toBe('active');
  });
});
