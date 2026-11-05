import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import { appEndpoints } from '@/lib/api/endpoints';
import type { App, Dash } from '@/lib/api/endpoints/schemas';
import { appQueryOptions, workspaceQueryKeys } from '@/lib/workspace/queries';
import type { AppStatus, AppTeamRole } from './settings';

/**
 * C.16 data: the enriched app record (members, invites and the admin token when
 * the caller may see it) plus the mutations for every settings tab.
 *
 * Mutations write the acknowledged change into both cached app shapes — the
 * per-app query and the bootstrap list — so the header and tables agree without
 * a refetch racing the next action.
 */

/** Writes an app change into both cached app shapes (queries and bootstrap). */
export function updateCachedApp(
  queryClient: QueryClient,
  appId: string,
  update: (app: App) => App,
): void {
  queryClient.setQueryData<{ app: App }>(
    workspaceQueryKeys.app(appId),
    (current) =>
      current === undefined ? current : { app: update(current.app) },
  );
  queryClient.setQueryData<Dash>(workspaceQueryKeys.bootstrap, (current) =>
    current === undefined
      ? current
      : {
          ...current,
          apps: current.apps.map((app) =>
            app.id === appId ? update(app) : app,
          ),
        },
  );
}

/** `GET /dash/apps/:id`; the layout already has a plain app for the header. */
export function useAppDetails(appId: string) {
  return useQuery(appQueryOptions(appId));
}

export function useRenameApp(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (title: string) =>
      apiFetch(appEndpoints.rename, { params: { appId }, body: { title } }),
    onSuccess: (_result, title) => {
      updateCachedApp(queryClient, appId, (app) => ({ ...app, title }));
    },
  });
}

export function useSetAppStatus(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (status: AppStatus) =>
      apiFetch(appEndpoints.setStatus, { params: { appId }, body: { status } }),
    onSuccess: (_result, status) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        status,
        effective_status: status,
      }));
    },
  });
}

/** D0.6: the server generates the token and the old one stops working at once. */
export function useRotateAppAdminToken(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () =>
      apiFetch(appEndpoints.rotateAdminToken, { params: { appId } }),
    onSuccess: (result) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        admin_token: result.adminToken,
      }));
    },
  });
}

export interface InviteAppMemberVariables {
  email: string;
  role: AppTeamRole;
}

export function useInviteAppMember(appId: string) {
  return useMutation({
    mutationFn: ({ email, role }: InviteAppMemberVariables) =>
      apiFetch(appEndpoints.inviteMember, {
        params: { appId },
        body: { 'invitee-email': email, role },
      }),
    // The server picks the invite id, so the member list is refetched.
    meta: { invalidates: [workspaceQueryKeys.app(appId)] },
  });
}

export function useRevokeAppInvite(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (inviteId: string) =>
      apiFetch(appEndpoints.revokeInvite, {
        params: { appId },
        body: { 'invite-id': inviteId },
      }),
    onSuccess: (_result, inviteId) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        invites: (app.invites ?? []).filter((invite) => invite.id !== inviteId),
      }));
    },
  });
}

export interface UpdateAppMemberVariables {
  memberId: string;
  role: AppTeamRole;
}

export function useUpdateAppMember(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ memberId, role }: UpdateAppMemberVariables) =>
      apiFetch(appEndpoints.updateMember, {
        params: { appId },
        body: { id: memberId, role },
      }),
    onSuccess: (_result, { memberId, role }) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        members: (app.members ?? []).map((member) =>
          member.id === memberId ? { ...member, role } : member,
        ),
      }));
    },
  });
}

export function useRemoveAppMember(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (memberId: string) =>
      apiFetch(appEndpoints.removeMember, {
        params: { appId },
        body: { id: memberId },
      }),
    onSuccess: (_result, memberId) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        members: (app.members ?? []).filter((member) => member.id !== memberId),
      }));
    },
  });
}

export function useTransferApp(appId: string) {
  return useMutation({
    mutationFn: (orgId: string) =>
      apiFetch(appEndpoints.transferToOrg, {
        params: { appId, orgId },
      }),
    meta: {
      invalidates: [
        workspaceQueryKeys.bootstrap,
        workspaceQueryKeys.app(appId),
      ],
    },
  });
}

export function useClearApp(appId: string) {
  return useMutation({
    mutationFn: () => apiFetch(appEndpoints.clear, { params: { appId } }),
    meta: { invalidates: [workspaceQueryKeys.app(appId)] },
  });
}

/** Deleting marks the app for the server's cleanup worker (C.16). */
export function useDeleteApp(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => apiFetch(appEndpoints.remove, { params: { appId } }),
    onSuccess: () => {
      queryClient.setQueryData<Dash>(workspaceQueryKeys.bootstrap, (current) =>
        current === undefined
          ? current
          : {
              ...current,
              apps: current.apps.filter((app) => app.id !== appId),
            },
      );
      queryClient.removeQueries({ queryKey: workspaceQueryKeys.app(appId) });
      void queryClient.invalidateQueries({
        queryKey: ['dash', 'apps-summary'],
      });
    },
  });
}
