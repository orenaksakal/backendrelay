import { queryOptions, useQuery } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  appEndpoints,
  orgEndpoints,
  sessionEndpoints,
} from '@/lib/api/endpoints';

/** A lookup with its own deadline never waits for the default 30s request cap. */
export const APP_LOOKUP_TIMEOUT_MS = 15_000;

export const workspaceQueryKeys = {
  bootstrap: ['dash'] as const,
  org: (orgId: string) => ['dash', 'orgs', orgId] as const,
  app: (appId: string) => ['dash', 'apps', appId] as const,
  /** Per-app usage roll-up for the apps table (D0.2). */
  summary: (workspaceId: string) =>
    ['dash', 'apps-summary', workspaceId] as const,
};

/**
 * `GET /dash` is the single bootstrap payload: user, personal apps, orgs,
 * pending invites, superuser and the config app id.
 */
export function bootstrapQueryOptions() {
  return queryOptions({
    queryKey: workspaceQueryKeys.bootstrap,
    queryFn: ({ signal }) => apiFetch(sessionEndpoints.bootstrap, { signal }),
  });
}

/** Org apps, members and invites come from `GET /dash/orgs/:id` (C.4, C.17). */
export function orgQueryOptions(orgId: string) {
  return queryOptions({
    queryKey: workspaceQueryKeys.org(orgId),
    queryFn: ({ signal }) =>
      apiFetch(orgEndpoints.get, { params: { orgId }, signal }),
  });
}

/** Used only when an app is missing from the bootstrap payload (D3.3). */
export function appQueryOptions(appId: string) {
  return queryOptions({
    queryKey: workspaceQueryKeys.app(appId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.get, {
        params: { appId },
        signal,
        timeoutMs: APP_LOOKUP_TIMEOUT_MS,
      }),
  });
}

/**
 * One request per workspace for the whole apps table: sessions, bytes, the
 * last write and the last backup for every app the caller can see (D0.2).
 */
export function appsSummaryQueryOptions(workspaceId: string) {
  return queryOptions({
    queryKey: workspaceQueryKeys.summary(workspaceId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.summary, {
        params: { workspace: workspaceId },
        signal,
      }),
  });
}

export function useBootstrap() {
  return useQuery(bootstrapQueryOptions());
}

/** Org pages pass `null` while the workspace id is still resolving. */
export function useOrgDetails(orgId: string | null) {
  return useQuery({
    ...orgQueryOptions(orgId ?? ''),
    enabled: orgId !== null,
  });
}

/** Apps home asks for the workspace summary that fills every metric column. */
export function useAppsSummary(workspaceId: string | null) {
  return useQuery({
    ...appsSummaryQueryOptions(workspaceId ?? ''),
    enabled: workspaceId !== null,
  });
}
