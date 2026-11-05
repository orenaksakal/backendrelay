import { describe, expect, it } from 'vitest';
import type { OrgSummary } from '@/lib/api/endpoints/schemas';
import {
  APP_SETTINGS_TABS,
  appInviteProblem,
  appMemberRoleLabel,
  appSettingsSearchSchema,
  assignableAppRoles,
  canClearAppData,
  canDeleteApp,
  canManageAppTeam,
  canPauseWrites,
  canRenameApp,
  canTransferApp,
  canViewAdminToken,
  isAppSettingsTab,
  isWritesPaused,
  statusForPause,
  transferableOrgs,
} from './settings';

const orgs: OrgSummary[] = [
  {
    id: 'org-admin',
    title: 'Acme',
    role: 'admin',
    created_at: '',
    paid: false,
  },
  {
    id: 'org-collab',
    title: 'Friends',
    role: 'collaborator',
    created_at: '',
    paid: false,
  },
  {
    id: 'org-owner',
    title: 'Mine',
    role: 'owner',
    created_at: '',
    paid: false,
  },
];

describe('app settings model', () => {
  it('parses the tab search param and falls back to general', () => {
    expect(appSettingsSearchSchema.parse({ tab: 'team' })).toEqual({
      tab: 'team',
    });
    expect(appSettingsSearchSchema.parse({ tab: 'nope' })).toEqual({
      tab: 'general',
    });
    expect(appSettingsSearchSchema.parse({})).toEqual({ tab: 'general' });
    for (const tab of APP_SETTINGS_TABS) {
      expect(isAppSettingsTab(tab)).toBe(true);
    }
    expect(isAppSettingsTab('unknown')).toBe(false);
  });

  it('maps each control to the role the server requires', () => {
    expect(canRenameApp('owner')).toBe(true);
    expect(canRenameApp('admin')).toBe(false);
    expect(canTransferApp('owner')).toBe(true);
    expect(canTransferApp('admin')).toBe(false);
    expect(canClearAppData('owner')).toBe(true);
    expect(canClearAppData('admin')).toBe(false);

    for (const role of ['owner', 'admin'] as const) {
      expect(canPauseWrites(role)).toBe(true);
      expect(canViewAdminToken(role)).toBe(true);
      expect(canManageAppTeam(role)).toBe(true);
      expect(assignableAppRoles(role)).toEqual(['admin', 'collaborator']);
    }

    expect(canPauseWrites('collaborator')).toBe(false);
    expect(canManageAppTeam('app-member')).toBe(false);
    expect(assignableAppRoles('collaborator')).toEqual([]);
  });

  it('needs the owner to delete a personal app but an admin in an org', () => {
    expect(canDeleteApp('owner', 'personal')).toBe(true);
    expect(canDeleteApp('admin', 'personal')).toBe(false);
    expect(canDeleteApp('collaborator', 'personal')).toBe(false);
    expect(canDeleteApp('admin', 'org-1')).toBe(true);
    expect(canDeleteApp('owner', 'org-1')).toBe(true);
    expect(canDeleteApp('collaborator', 'org-1')).toBe(false);
  });

  it('toggles writes between active and read-only', () => {
    expect(statusForPause(true)).toBe('read-only');
    expect(statusForPause(false)).toBe('active');
    expect(isWritesPaused('read-only')).toBe(true);
    expect(isWritesPaused('active')).toBe(false);
    expect(isWritesPaused('disabled')).toBe(false);
  });

  it('offers only orgs the actor administers and skips the current one', () => {
    expect(transferableOrgs(orgs, 'org-admin').map((org) => org.id)).toEqual([
      'org-owner',
    ]);
    expect(transferableOrgs(orgs, undefined).map((org) => org.id)).toEqual([
      'org-admin',
      'org-owner',
    ]);
    expect(transferableOrgs(undefined, undefined)).toEqual([]);
  });

  it('names member roles, including the app creator', () => {
    expect(appMemberRoleLabel('owner')).toBe('Owner');
    expect(appMemberRoleLabel('admin')).toBe('Admin');
    expect(appMemberRoleLabel('collaborator')).toBe('Collaborator');
  });

  it('rejects invalid invitations before the request', () => {
    const base = {
      role: 'collaborator' as const,
      myRole: 'admin' as const,
      members: [{ email: 'owner@example.com' }],
      invites: [
        { email: 'pending@example.com', status: 'pending', expired: false },
      ],
    };

    expect(appInviteProblem({ ...base, email: 'new@example.com' })).toBe(
      undefined,
    );
    expect(appInviteProblem({ ...base, email: ' ' })).toBe(
      'Enter an email address.',
    );
    expect(appInviteProblem({ ...base, email: 'nope' })).toBe(
      'Enter a valid email address.',
    );
    expect(appInviteProblem({ ...base, email: 'owner@example.com' })).toBe(
      'This person is already a member.',
    );
    expect(appInviteProblem({ ...base, email: 'pending@example.com' })).toBe(
      'This person already has a pending invitation.',
    );
    expect(
      appInviteProblem({
        ...base,
        myRole: 'collaborator',
        email: 'new@example.com',
      }),
    ).toBe('App administrators and owners can invite members.');
  });
});
