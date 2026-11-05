import type { EffectiveRole } from '@/lib/api/endpoints/schemas';

/**
 * C.17 organization roles. `app-member` is a visibility role on
 * `GET /dash/orgs/:id`, not a role the org itself can grant.
 */
export type OrgRole = 'owner' | 'admin' | 'collaborator';

/** Most privileged first, which is the order the role selector lists. */
export const ORG_ROLES: readonly OrgRole[] = ['owner', 'admin', 'collaborator'];

export const ORG_ROLE_LABELS: Record<OrgRole, string> = {
  owner: 'Owner',
  admin: 'Admin',
  collaborator: 'Collaborator',
};

const ROLE_RANK: Record<EffectiveRole, number> = {
  owner: 4,
  admin: 3,
  collaborator: 2,
  'app-member': 1,
};

export function isOrgRole(value: unknown): value is OrgRole {
  return value === 'owner' || value === 'admin' || value === 'collaborator';
}

export function roleRank(role: EffectiveRole): number {
  return ROLE_RANK[role];
}

/** Administrators and owners may invite, revoke invitations and change roles. */
export function canManageOrg(
  role: EffectiveRole | undefined,
): role is EffectiveRole {
  return role !== undefined && roleRank(role) >= roleRank('admin');
}

/** Only owners may delete the organization (C.17 danger zone). */
export function canDeleteOrg(role: EffectiveRole | undefined): boolean {
  return role === 'owner';
}

/**
 * The roles the actor may give a member who currently has `theirRole`. Never
 * above the actor's own role, and collaborators cannot change roles at all.
 */
export function assignableRoles(
  myRole: EffectiveRole,
  theirRole: OrgRole,
): OrgRole[] {
  if (!canManageOrg(myRole) || roleRank(theirRole) > roleRank(myRole)) {
    return [];
  }
  return ORG_ROLES.filter((role) => roleRank(role) <= roleRank(myRole));
}

/** Roles an actor may attach to a new invitation. */
export function inviteRoles(myRole: EffectiveRole | undefined): OrgRole[] {
  if (!canManageOrg(myRole)) return [];
  return ORG_ROLES.filter((role) => roleRank(role) <= roleRank(myRole));
}

export interface OrgMemberIdentity {
  id: string;
  role: OrgRole;
}

/** The database trigger refuses to leave an organization without an owner. */
export function isLastOwner(
  members: readonly OrgMemberIdentity[],
  memberId: string,
): boolean {
  return (
    members.some(
      (member) => member.id === memberId && member.role === 'owner',
    ) && members.filter((member) => member.role === 'owner').length <= 1
  );
}

export interface RemovalPermission {
  myRole: EffectiveRole;
  theirRole: OrgRole;
  lastOwner: boolean;
}

/** Mirrors the server's rank check, with the last-owner guard on top. */
export function canRemoveMember({
  myRole,
  theirRole,
  lastOwner,
}: RemovalPermission): boolean {
  return (
    canManageOrg(myRole) &&
    roleRank(theirRole) <= roleRank(myRole) &&
    !lastOwner
  );
}

const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export interface InviteCheck {
  email: string;
  role: OrgRole;
  myRole: EffectiveRole;
  members: ReadonlyArray<{ email: string }>;
  invites: ReadonlyArray<{ email: string; status: string; expired: boolean }>;
}

/** Client-side checks that keep an invalid invitation off the wire. */
export function inviteProblem({
  email,
  role,
  myRole,
  members,
  invites,
}: InviteCheck): string | undefined {
  if (!canManageOrg(myRole)) {
    return 'Organization administrators or owners can invite members.';
  }
  if (!inviteRoles(myRole).includes(role)) {
    return 'You cannot invite this role.';
  }
  const normalized = email.trim().toLowerCase();
  if (normalized === '') return 'Enter an email address.';
  if (!EMAIL_PATTERN.test(normalized)) return 'Enter a valid email address.';
  if (members.some((member) => member.email.toLowerCase() === normalized)) {
    return 'This person is already a member.';
  }
  if (
    invites.some(
      (invite) =>
        invite.status === 'pending' &&
        !invite.expired &&
        invite.email.toLowerCase() === normalized,
    )
  ) {
    return 'This person already has a pending invitation.';
  }
  return undefined;
}
