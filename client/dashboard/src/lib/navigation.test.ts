import { describe, expect, it } from 'vitest';
import {
  accountNavigation,
  activeNavItem,
  appHref,
  appNavigation,
  documentTitle,
  navLandmarkLabel,
  operatorNavigation,
  pageLabelFromPath,
  workspaceIdFromPath,
  workspaceNavigation,
} from './navigation';

describe('appNavigation', () => {
  it('lists every section in the B.6 order', () => {
    const navigation = appNavigation('app-1');
    const items = navigation.groups.flatMap((group) => group.items);

    expect(items.map((item) => item.label)).toEqual([
      'Overview',
      'Explorer',
      'Schema',
      'Users',
      'Storage',
      'Permissions',
      'Auth',
      'Playground',
      'Webhooks',
      'OAuth apps',
      'Backups',
      'Settings',
    ]);
    expect(items.map((item) => item.href)).toContain('/apps/app-1/explorer');
    expect(navigation.groups.map((group) => group.label)).toEqual([
      undefined,
      'Data',
      'Access',
      'Build',
      'Operate',
    ]);
  });

  it('keeps the current section when switching apps', () => {
    expect(appHref('app-2', 'webhooks')).toBe('/apps/app-2/webhooks');
    expect(appHref('app-2', 'missing')).toBe('/apps/app-2/overview');
  });

  it('matches detail pages to their section but not extra overview paths', () => {
    const navigation = appNavigation('app-1');

    const webhook = activeNavItem(navigation, '/apps/app-1/webhooks/hook-1');
    expect(webhook?.id).toBe('webhooks');
    const overview = activeNavItem(navigation, '/apps/app-1/overview');
    expect(overview?.id).toBe('overview');
    const extra = activeNavItem(navigation, '/apps/app-1/overview/extra');
    expect(extra).toBeUndefined();
  });
});

describe('workspaceNavigation', () => {
  it('shows only Apps for the personal workspace', () => {
    const navigation = workspaceNavigation({
      workspaceId: 'personal',
      isOrg: false,
    });

    const items = navigation.groups.flatMap((group) => group.items);
    expect(items.map((item) => item.id)).toEqual(['apps']);
  });

  it('adds Members and Settings for an org', () => {
    const navigation = workspaceNavigation({
      workspaceId: 'org-1',
      isOrg: true,
    });

    const items = navigation.groups.flatMap((group) => group.items);
    expect(items.map((item) => item.href)).toEqual([
      '/w/org-1',
      '/w/org-1/members',
      '/w/org-1/settings',
    ]);
  });
});

describe('account and operator navigation', () => {
  it('names each nav landmark for the area', () => {
    expect(navLandmarkLabel(accountNavigation().area)).toBe('Account');
    expect(navLandmarkLabel(operatorNavigation().area)).toBe('Operator');
  });
});

describe('path helpers', () => {
  it('reads and decodes the workspace id', () => {
    expect(workspaceIdFromPath('/w/personal')).toBe('personal');
    expect(workspaceIdFromPath('/w/org%201/settings')).toBe('org 1');
    expect(workspaceIdFromPath('/apps/app-1/overview')).toBeNull();
  });

  it('humanises the last segment for screens outside the nav model', () => {
    expect(pageLabelFromPath('/welcome')).toBe('Welcome');
    expect(pageLabelFromPath('/new')).toBe('New');
  });
});

describe('documentTitle', () => {
  it('renders page, app and brand', () => {
    expect(documentTitle('Overview', 'todo-app')).toBe(
      'Overview · todo-app · BackendRelay',
    );
    expect(documentTitle('Account', undefined)).toBe('Account · BackendRelay');
    expect(documentTitle(undefined, undefined)).toBe('BackendRelay');
  });
});
