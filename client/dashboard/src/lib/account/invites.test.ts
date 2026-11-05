import { describe, expect, it } from 'vitest';
import { inviteDestination, inviteTypeLabel } from './invites';

describe('invite destinations', () => {
  it('sends org invites to the workspace page', () => {
    expect(inviteDestination({ type: 'org', foreign_key: 'org-1' })).toBe(
      '/w/org-1',
    );
  });

  it('sends app invites to the app overview', () => {
    expect(inviteDestination({ type: 'app', foreign_key: 'app-1' })).toBe(
      '/apps/app-1/overview',
    );
  });

  it('encodes ids and labels invite types', () => {
    expect(inviteDestination({ type: 'app', foreign_key: 'a b/c' })).toBe(
      '/apps/a%20b%2Fc/overview',
    );
    expect(inviteTypeLabel('org')).toBe('Organization');
    expect(inviteTypeLabel('app')).toBe('App');
  });
});
