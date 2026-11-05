import { describe, expect, it } from 'vitest';
import type { Dash } from '@/lib/api/endpoints/schemas';
import {
  PERSONAL_WORKSPACE_ID,
  landingPath,
  listWorkspaces,
} from './workspaces';

function dash(overrides: Partial<Dash> = {}): Dash {
  return {
    apps: overrides.apps ?? [],
    orgs: overrides.orgs ?? [],
    invites: overrides.invites ?? [],
    superuser: overrides.superuser ?? false,
    user: overrides.user ?? { id: 'user-1', email: 'owner@example.com' },
  };
}

const org = {
  id: 'org-1',
  title: 'Acme',
  created_at: '2026-01-01T00:00:00Z',
  role: 'owner' as const,
  paid: false,
};

describe('listWorkspaces', () => {
  it('lists Personal first and then every org', () => {
    const workspaces = listWorkspaces(dash({ orgs: [org] }));

    expect(workspaces.map((workspace) => workspace.id)).toEqual([
      PERSONAL_WORKSPACE_ID,
      'org-1',
    ]);
    expect(workspaces[0]).toMatchObject({
      title: 'Personal',
      kind: 'personal',
    });
    expect(workspaces[1]).toMatchObject({
      title: 'Acme',
      kind: 'org',
      role: 'owner',
    });
  });

  it('handles a payload without orgs', () => {
    expect(listWorkspaces(undefined)).toHaveLength(1);
  });
});

describe('landingPath', () => {
  it('sends a first-run account to /welcome', () => {
    expect(landingPath(dash(), null)).toBe('/welcome');
  });

  it('sends an invited account with nothing else to /account/invites', () => {
    const invites = [
      {
        type: 'org' as const,
        id: 'invite-1',
        foreign_key: 'org-1',
        title: 'Acme',
        invitee_role: 'collaborator' as const,
        inviter_email: 'admin@example.com',
      },
    ];

    expect(landingPath(dash({ invites }), null)).toBe('/account/invites');
  });

  it('returns to the remembered workspace', () => {
    const payload = dash({ orgs: [org] });

    expect(landingPath(payload, 'org-1')).toBe('/w/org-1');
  });

  it('falls back to Personal for an unknown saved workspace', () => {
    expect(landingPath(dash({ orgs: [org] }), 'deleted-org')).toBe(
      '/w/personal',
    );
  });

  it('uses Personal when the account only has personal apps', () => {
    const payload = dash({
      apps: [
        {
          id: 'app-1',
          title: 'todo-app',
          created_at: '2026-01-01T00:00:00Z',
        },
      ],
    });

    expect(landingPath(payload, null)).toBe('/w/personal');
  });
});
