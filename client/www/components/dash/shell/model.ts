export function singleQuery(
  value: string | string[] | undefined,
): string | null {
  return typeof value === 'string' && value ? value : null;
}
export function initialWorkspace(urlOrg: string | null, saved: string | null) {
  return urlOrg || saved || 'personal';
}
export type WorkspaceSelection = { id: string; urlOrg: string | null };
export function resolveWorkspace(
  selection: WorkspaceSelection,
  urlOrg: string | null,
) {
  return selection.urlOrg !== urlOrg ? urlOrg || 'personal' : selection.id;
}
export function workspaceHref(
  pathname: string,
  workspaceId: string,
  values: Record<string, string | undefined> = {},
) {
  const params = new URLSearchParams();
  if (workspaceId !== 'personal') params.set('org', workspaceId);
  for (const [key, value] of Object.entries(values))
    if (value && key !== 'org') params.set(key, value);
  return `${pathname}${params.size ? `?${params}` : ''}`;
}
export function emptyWorkspaceDestination(
  apps: number,
  invites: number,
  orgs: number,
) {
  if (apps) return null;
  if (invites) return '/dash/user-settings';
  if (!orgs) return '/dash/onboarding';
  return '/dash/new';
}
export function filterWorkspaces<
  T extends { id: string; title: string; role: string },
>(workspaces: T[], search: string) {
  const term = search.trim().toLowerCase();
  return workspaces
    .filter((w) =>
      [w.title, w.id, w.role].some((v) => v.toLowerCase().includes(term)),
    )
    .sort((a, b) => a.title.localeCompare(b.title));
}
export function workspaceCacheKey(
  token: string | undefined | null,
  id: string,
) {
  return token && id !== 'personal'
    ? (['dashboard-workspace', token, id] as const)
    : null;
}

export function connectionMatches(
  connection: { appId: string; adminToken: string } | null,
  appId: string,
  adminToken: string,
) {
  return (
    !!connection &&
    connection.appId === appId &&
    connection.adminToken === adminToken
  );
}
