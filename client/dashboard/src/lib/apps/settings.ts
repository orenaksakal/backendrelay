import { z } from 'zod';
import type {
  App,
  EffectiveRole,
  OrgSummary,
} from '@/lib/api/endpoints/schemas';
import { roleRank } from '@/lib/organization/model';
import { PERSONAL_WORKSPACE_ID } from '@/lib/workspace/workspaces';

/**
 * C.16 app settings model: the `?tab=` contract, the role that each control
 * needs (mirroring the server's checks) and the invite validation that keeps an
 * obviously invalid invitation off the wire.
 */

export const APP_SETTINGS_TABS = [
  'general',
  'token',
  'team',
  'danger',
] as const;

export type AppSettingsTab = (typeof APP_SETTINGS_TABS)[number];

export function isAppSettingsTab(value: unknown): value is AppSettingsTab {
  return (
    typeof value === 'string' &&
    (APP_SETTINGS_TABS as readonly string[]).includes(value)
  );
}

export const APP_SETTINGS_TAB_LABELS: Record<AppSettingsTab, string> = {
  general: 'General',
  token: 'Admin token',
  team: 'Team',
  danger: 'Danger zone',
};

export const appSettingsSearchSchema = z.object({
  tab: z.enum(APP_SETTINGS_TABS).catch('general'),
});

export type AppSettingsSearch = z.infer<typeof appSettingsSearchSchema>;

/** App membership roles the dashboard grants; the app creator stays an owner. */
export const APP_TEAM_ROLES = ['admin', 'collaborator'] as const;

export type AppTeamRole = (typeof APP_TEAM_ROLES)[number];

export const APP_TEAM_ROLE_LABELS: Record<AppTeamRole, string> = {
  admin: 'Admin',
  collaborator: 'Collaborator',
};

export function isAppTeamRole(value: unknown): value is AppTeamRole {
  return (
    typeof value === 'string' &&
    (APP_TEAM_ROLES as readonly string[]).includes(value)
  );
}

/** The app creator's effective role. Only owners may rename or transfer. */
export function isAppOwner(role: EffectiveRole): boolean {
  return role === 'owner';
}

/** Administrators and owners may rotate the token, pause writes and manage the team. */
export function canAdministerApp(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('admin');
}

export function canRenameApp(role: EffectiveRole): boolean {
  return isAppOwner(role);
}

export function canPauseWrites(role: EffectiveRole): boolean {
  return canAdministerApp(role);
}

/** `GET /dash/apps/:id` only returns the admin token to admins and owners. */
export function canViewAdminToken(role: EffectiveRole): boolean {
  return canAdministerApp(role);
}

export function canManageAppTeam(role: EffectiveRole): boolean {
  return canAdministerApp(role);
}

export function canTransferApp(role: EffectiveRole): boolean {
  return isAppOwner(role);
}

/** D0.7 makes clearing an app's data an owner-only action. */
export function canClearAppData(role: EffectiveRole): boolean {
  return isAppOwner(role);
}

/**
 * The server lets admins delete org apps but requires the creator for a
 * personally owned app (C.16).
 */
export function canDeleteApp(
  role: EffectiveRole,
  workspaceId: string,
): boolean {
  if (workspaceId === PERSONAL_WORKSPACE_ID) return isAppOwner(role);
  return canAdministerApp(role);
}

/** Roles an administrator may attach to an app member or invitation. */
export function assignableAppRoles(role: EffectiveRole): AppTeamRole[] {
  return canManageAppTeam(role) ? [...APP_TEAM_ROLES] : [];
}

export function appMemberRoleLabel(role: string): string {
  if (role === 'owner' || role === 'creator') return 'Owner';
  if (role === 'admin') return 'Admin';
  if (role === 'collaborator') return 'Collaborator';
  return role;
}

export type AppStatus = NonNullable<App['status']>;

/** The status the Pause writes switch writes for its checked state. */
export function statusForPause(paused: boolean): AppStatus {
  return paused ? 'read-only' : 'active';
}

export function isWritesPaused(status: AppStatus): boolean {
  return status === 'read-only';
}

export interface TransferTarget {
  id: string;
  title: string;
  role: EffectiveRole;
}

/**
 * Orgs the actor administers, minus the app's current org. Transferring needs
 * an admin role in the target (C.16).
 */
export function transferableOrgs(
  orgs: readonly OrgSummary[] | undefined,
  currentOrgId: string | null | undefined,
): TransferTarget[] {
  return (orgs ?? [])
    .filter(
      (org) =>
        org.id !== currentOrgId && roleRank(org.role) >= roleRank('admin'),
    )
    .map((org) => ({ id: org.id, title: org.title, role: org.role }));
}

const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export interface AppInviteCheck {
  email: string;
  role: AppTeamRole;
  myRole: EffectiveRole;
  members: ReadonlyArray<{ email: string }>;
  invites: ReadonlyArray<{ email: string; status: string; expired: boolean }>;
}

/** Client-side checks before `POST …/invite/send` (C.16). */
export function appInviteProblem({
  email,
  role,
  myRole,
  members,
  invites,
}: AppInviteCheck): string | undefined {
  if (!canManageAppTeam(myRole)) {
    return 'App administrators and owners can invite members.';
  }
  if (!assignableAppRoles(myRole).includes(role)) {
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
