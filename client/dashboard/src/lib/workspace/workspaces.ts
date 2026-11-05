import type {
  Dash,
  EffectiveRole,
  OrgSummary,
} from '@/lib/api/endpoints/schemas';

/** The personal workspace is not an org; its id is reserved in URLs. */
export const PERSONAL_WORKSPACE_ID = 'personal';
export const PERSONAL_WORKSPACE_TITLE = 'Personal';

/** Remembered so `/` returns to the workspace the user last opened (D3.3). */
export const LAST_WORKSPACE_STORAGE_KEY = 'br.lastWorkspace';

export type WorkspaceKind = 'personal' | 'org';

export interface Workspace {
  id: string;
  title: string;
  kind: WorkspaceKind;
  role: EffectiveRole;
}

/**
 * Personal first, then the orgs in the order `/dash` returned them. App-member
 * orgs stay in the list because they can still own apps the user can open; the
 * role tells the org pages whether to allow changes.
 */
export function listWorkspaces(
  dash: Pick<Dash, 'orgs'> | null | undefined,
): Workspace[] {
  const orgs: OrgSummary[] = dash?.orgs ?? [];
  return [
    {
      id: PERSONAL_WORKSPACE_ID,
      title: PERSONAL_WORKSPACE_TITLE,
      kind: 'personal',
      role: 'owner',
    },
    ...orgs.map((org) => ({
      id: org.id,
      title: org.title,
      kind: 'org' as const,
      role: org.role,
    })),
  ];
}

export function findWorkspace(
  workspaces: Workspace[],
  workspaceId: string,
): Workspace | undefined {
  return workspaces.find((workspace) => workspace.id === workspaceId);
}

/** True when the user has no apps, no orgs and no invites (C.3 `/welcome`). */
export function isFirstRun(
  dash: Pick<Dash, 'apps' | 'orgs' | 'invites'>,
): boolean {
  return (
    dash.apps.length === 0 &&
    (dash.orgs?.length ?? 0) === 0 &&
    (dash.invites?.length ?? 0) === 0
  );
}

/**
 * Where `/` sends the user: pending invites for an empty account, otherwise the
 * last workspace they opened or Personal. A stale saved id falls back to
 * Personal instead of a workspace that no longer exists.
 */
export function landingPath(
  dash: Pick<Dash, 'apps' | 'orgs' | 'invites'>,
  lastWorkspace: string | null,
): string {
  const workspaces = listWorkspaces(dash);
  if (isFirstRun(dash)) return '/welcome';

  const hasApps = dash.apps.length > 0;
  const hasOrgs = workspaces.length > 1;
  if (!hasApps && !hasOrgs) return '/account/invites';

  const remembered =
    lastWorkspace !== null &&
    workspaces.some((workspace) => workspace.id === lastWorkspace)
      ? lastWorkspace
      : PERSONAL_WORKSPACE_ID;
  return `/w/${encodeURIComponent(remembered)}`;
}

export function readLastWorkspace(
  storage: Storage | null = storageOrNull(),
): string | null {
  if (storage === null) return null;
  try {
    const value = storage.getItem(LAST_WORKSPACE_STORAGE_KEY);
    return value === null || value === '' ? null : value;
  } catch {
    return null;
  }
}

export function rememberLastWorkspace(
  workspaceId: string,
  storage: Storage | null = storageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(LAST_WORKSPACE_STORAGE_KEY, workspaceId);
  } catch {
    /* Navigation still works when storage is blocked or full. */
  }
}

function storageOrNull(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}
