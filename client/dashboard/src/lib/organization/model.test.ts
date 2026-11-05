import { describe, expect, it } from 'vitest';
import {
  ORG_ROLE_LABELS,
  assignableRoles,
  canDeleteOrg,
  canManageOrg,
  canRemoveMember,
  inviteProblem,
  inviteRoles,
  isLastOwner,
  isOrgRole,
  roleRank,
} from './model';

const members = [
  { id: 'owner', email: 'owner@example.test', role: 'owner' as const },
  { id: 'admin', email: 'admin@example.test', role: 'admin' as const },
  {
    id: 'collaborator',
    email: 'collab@example.test',
    role: 'collaborator' as const,
  },
];

const invites = [
  {
    id: 'invite',
    email: 'invited@example.test',
    status: 'pending',
    expired: false,
  },
];

describe('organization roles', () => {
  it('ranks the membership roles and recognizes them on the wire', () => {
    expect(roleRank('owner')).toBeGreaterThan(roleRank('admin'));
    expect(roleRank('admin')).toBeGreaterThan(roleRank('collaborator'));
    expect(roleRank('collaborator')).toBeGreaterThan(roleRank('app-member'));
    expect(isOrgRole('admin')).toBe(true);
    expect(isOrgRole('app-member')).toBe(false);
    expect(ORG_ROLE_LABELS.collaborator).toBe('Collaborator');
  });

  it('only lets administrators and owners manage the organization', () => {
    expect(canManageOrg('owner')).toBe(true);
    expect(canManageOrg('admin')).toBe(true);
    expect(canManageOrg('collaborator')).toBe(false);
    expect(canManageOrg('app-member')).toBe(false);
    expect(canManageOrg(undefined)).toBe(false);
    expect(canDeleteOrg('owner')).toBe(true);
    expect(canDeleteOrg('admin')).toBe(false);
  });

  it("caps role changes at the actor's own role", () => {
    expect(assignableRoles('collaborator', 'collaborator')).toEqual([]);
    expect(assignableRoles('admin', 'owner')).toEqual([]);
    expect(assignableRoles('admin', 'collaborator')).toEqual([
      'admin',
      'collaborator',
    ]);
    expect(assignableRoles('owner', 'admin')).toEqual([
      'owner',
      'admin',
      'collaborator',
    ]);
    expect(inviteRoles('admin')).toEqual(['admin', 'collaborator']);
    expect(inviteRoles('owner')).toEqual(['owner', 'admin', 'collaborator']);
    expect(inviteRoles('collaborator')).toEqual([]);
  });
});

describe('last owner and removal', () => {
  it('protects the only owner from demotion and removal', () => {
    expect(isLastOwner(members, 'owner')).toBe(true);
    expect(isLastOwner(members, 'admin')).toBe(false);
    expect(
      canRemoveMember({
        myRole: 'owner',
        theirRole: 'owner',
        lastOwner: true,
      }),
    ).toBe(false);
    expect(
      canRemoveMember({
        myRole: 'admin',
        theirRole: 'owner',
        lastOwner: false,
      }),
    ).toBe(false);
    expect(
      canRemoveMember({
        myRole: 'admin',
        theirRole: 'admin',
        lastOwner: false,
      }),
    ).toBe(true);
  });

  it('allows the last owner to go once a second owner exists', () => {
    const twoOwners = [
      ...members,
      { id: 'second', email: 'second@example.test', role: 'owner' as const },
    ];
    expect(isLastOwner(twoOwners, 'owner')).toBe(false);
    expect(
      canRemoveMember({
        myRole: 'owner',
        theirRole: 'owner',
        lastOwner: false,
      }),
    ).toBe(true);
  });
});

describe('invite checks', () => {
  it('refuses roles above the actor and invalid addresses', () => {
    expect(
      inviteProblem({
        email: 'new@example.test',
        role: 'owner',
        myRole: 'admin',
        members,
        invites,
      }),
    ).toBe('You cannot invite this role.');
    expect(
      inviteProblem({
        email: 'not-an-email',
        role: 'collaborator',
        myRole: 'admin',
        members,
        invites,
      }),
    ).toBe('Enter a valid email address.');
    expect(
      inviteProblem({
        email: 'new@example.test',
        role: 'collaborator',
        myRole: 'collaborator',
        members,
        invites,
      }),
    ).toBe('Organization administrators or owners can invite members.');
  });

  it('refuses existing members and pending invitations', () => {
    expect(
      inviteProblem({
        email: ' ADMIN@example.test ',
        role: 'admin',
        myRole: 'admin',
        members,
        invites,
      }),
    ).toBe('This person is already a member.');
    expect(
      inviteProblem({
        email: 'invited@example.test',
        role: 'collaborator',
        myRole: 'admin',
        members,
        invites,
      }),
    ).toBe('This person already has a pending invitation.');
    expect(
      inviteProblem({
        email: 'new@example.test',
        role: 'collaborator',
        myRole: 'admin',
        members,
        invites,
      }),
    ).toBeUndefined();
  });
});
