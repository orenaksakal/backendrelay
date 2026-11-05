import { describe, expect, it } from 'vitest';
import { isLegacyDashIndex, legacyRedirect } from './legacy-redirects';

/**
 * Every row of the B.6 legacy redirect table. The old URLs are kept verbatim
 * so a reviewer can diff this list against the plan.
 */
const legacyRedirectTable: Array<[from: string, to: string]> = [
  // CLI login tickets.
  ['/dash?ticket=ticket-1', '/cli/approve?ticket=ticket-1'],
  ['/dash?_cli_oauth_ticket=ticket-1', '/cli/approve?ticket=ticket-1'],
  // Account screens.
  ['/dash?s=invites', '/account/invites'],
  ['/dash?s=personal-access-tokens', '/account/tokens'],
  ['/dash/user-settings?tab=token', '/account/tokens'],
  ['/dash/user-settings?tab=oauth', '/account/oauth-apps'],
  ['/dash/user-settings', '/account/profile'],
  ['/dash/user-settings?tab=other', '/account/profile'],
  // Creation and first run.
  ['/dash/new', '/new'],
  ['/dash?s=new', '/new'],
  ['/dash/onboarding', '/welcome'],
  // Organisations.
  ['/dash/org?org=org-1&tab=members', '/w/org-1/members'],
  ['/dash/org?org=org-1&tab=settings', '/w/org-1/settings'],
  ['/dash?org=org-1', '/w/org-1'],
  // App tabs, one row per mapping.
  ['/dash?app=app-1&s=main&t=home', '/apps/app-1/overview'],
  ['/dash?app=app-1&s=main&t=explorer', '/apps/app-1/explorer'],
  ['/dash?app=app-1&s=main&t=schema', '/apps/app-1/schema'],
  ['/dash?app=app-1&s=main&t=perms', '/apps/app-1/permissions'],
  ['/dash?app=app-1&s=main&t=auth', '/apps/app-1/auth'],
  ['/dash?app=app-1&s=main&t=webhooks', '/apps/app-1/webhooks'],
  ['/dash?app=app-1&s=main&t=backups', '/apps/app-1/backups'],
  ['/dash?app=app-1&s=main&t=repl', '/apps/app-1/playground'],
  ['/dash?app=app-1&s=main&t=sandbox', '/apps/app-1/playground'],
  ['/dash?app=app-1&s=main&t=admin', '/apps/app-1/settings'],
  ['/dash?app=app-1&s=main&t=billing', '/apps/app-1/settings'],
  ['/dash?app=app-1&s=main&t=oauth-apps', '/apps/app-1/oauth-apps'],
  ['/dash?app=app-1&s=main&t=unknown', '/apps/app-1/overview'],
  ['/dash?app=app-1', '/apps/app-1/overview'],
  // The dashboard root itself.
  ['/dash', '/'],
  // The old intern page.
  ['/intern/deployment-settings', '/operator/settings'],
];

describe('legacyRedirect', () => {
  it.each(legacyRedirectTable)('redirects %s to %s', (from, to) => {
    expect(legacyRedirect(from)).toBe(to);
  });

  it('lets an app win over an account screen, as the table specifies', () => {
    expect(legacyRedirect('/dash?app=app-1&s=invites&t=home')).toBe(
      '/apps/app-1/overview',
    );
  });

  it('lets a CLI ticket win over everything else on /dash', () => {
    expect(legacyRedirect('/dash?ticket=ticket-1&app=app-1')).toBe(
      '/cli/approve?ticket=ticket-1',
    );
  });

  it('normalises trailing slashes', () => {
    expect(legacyRedirect('/dash/')).toBe('/');
    expect(legacyRedirect('/dash/new/')).toBe('/new');
    expect(legacyRedirect('/intern/deployment-settings/')).toBe(
      '/operator/settings',
    );
  });

  it('sends unknown /dash subpaths to the dashboard root', () => {
    expect(legacyRedirect('/dash/unknown')).toBe('/');
  });

  it('encodes ids and tickets instead of concatenating raw values', () => {
    expect(legacyRedirect('/dash?app=app%20one&t=explorer')).toBe(
      '/apps/app%20one/explorer',
    );
    expect(legacyRedirect('/dash?ticket=a%2Fb%2Bc')).toBe(
      '/cli/approve?ticket=a%2Fb%2Bc',
    );
  });

  it('falls back to the dashboard root for /dash/org without an org', () => {
    expect(legacyRedirect('/dash/org?tab=members')).toBe('/');
  });
});

describe('isLegacyDashIndex', () => {
  it('matches only the dashboard root', () => {
    expect(isLegacyDashIndex('/dash')).toBe(true);
    expect(isLegacyDashIndex('/dash/')).toBe(true);
    expect(isLegacyDashIndex('/dash/new')).toBe(false);
  });
});
