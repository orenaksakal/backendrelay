import { useMutation, useQueryClient } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import { orgEndpoints } from '@/lib/api/endpoints';
import type { Dash, OrgDetails } from '@/lib/api/endpoints/schemas';
import type { OrgRole } from '@/lib/organization/model';
import { workspaceQueryKeys } from '@/lib/workspace/queries';

export interface UpdateMemberVariables {
  memberId: string;
  role: OrgRole;
}

/**
 * C.17 mutations. Each one writes the acknowledged change into the cached
 * `GET /dash/orgs/:id` payload, so the table updates without a refetch that
 * could race the next action.
 */
export function useRenameOrg(orgId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (title: string) =>
      apiFetch(orgEndpoints.rename, { params: { orgId }, body: { title } }),
    onSuccess: (_result, title) => {
      queryClient.setQueryData<OrgDetails>(
        workspaceQueryKeys.org(orgId),
        (current) =>
          current === undefined
            ? current
            : { ...current, org: { ...current.org, title } },
      );
      queryClient.setQueryData<Dash>(workspaceQueryKeys.bootstrap, (current) =>
        current === undefined
          ? current
          : {
              ...current,
              orgs: current.orgs?.map((org) =>
                org.id === orgId ? { ...org, title } : org,
              ),
            },
      );
    },
  });
}

/** Deleting an org also removes it from the workspace list and org cache. */
export function useDeleteOrg(orgId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => apiFetch(orgEndpoints.remove, { params: { orgId } }),
    onSuccess: () => {
      queryClient.setQueryData<Dash>(workspaceQueryKeys.bootstrap, (current) =>
        current === undefined
          ? current
          : {
              ...current,
              orgs: current.orgs?.filter((org) => org.id !== orgId),
            },
      );
      queryClient.removeQueries({ queryKey: workspaceQueryKeys.org(orgId) });
    },
  });
}

export function useInviteOrgMember(orgId: string) {
  return useMutation({
    mutationFn: (variables: { email: string; role: OrgRole }) =>
      apiFetch(orgEndpoints.inviteMember, {
        params: { orgId },
        body: { 'invitee-email': variables.email, role: variables.role },
      }),
    // The server picks the invite id, so the pending list is refetched.
    meta: { invalidates: [workspaceQueryKeys.org(orgId)] },
  });
}

export function useRevokeOrgInvite(orgId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (inviteId: string) =>
      apiFetch(orgEndpoints.revokeInvite, {
        params: { orgId },
        body: { 'invite-id': inviteId },
      }),
    onSuccess: (_result, inviteId) => {
      queryClient.setQueryData<OrgDetails>(
        workspaceQueryKeys.org(orgId),
        (current) =>
          current === undefined
            ? current
            : {
                ...current,
                invites: current.invites.filter(
                  (invite) => invite.id !== inviteId,
                ),
              },
      );
    },
  });
}

export function useUpdateOrgMember(orgId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ memberId, role }: UpdateMemberVariables) =>
      apiFetch(orgEndpoints.updateMember, {
        params: { orgId },
        body: { id: memberId, role },
      }),
    onSuccess: (_result, { memberId, role }) => {
      queryClient.setQueryData<OrgDetails>(
        workspaceQueryKeys.org(orgId),
        (current) =>
          current === undefined
            ? current
            : {
                ...current,
                members: current.members.map((member) =>
                  member.id === memberId ? { ...member, role } : member,
                ),
              },
      );
    },
  });
}

export function useRemoveOrgMember(orgId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (memberId: string) =>
      apiFetch(orgEndpoints.removeMember, {
        params: { orgId },
        body: { id: memberId },
      }),
    // Leaving changes the workspace switcher, so the bootstrap refreshes.
    meta: { invalidates: [workspaceQueryKeys.bootstrap] },
    onSuccess: (_result, memberId) => {
      queryClient.setQueryData<OrgDetails>(
        workspaceQueryKeys.org(orgId),
        (current) =>
          current === undefined
            ? current
            : {
                ...current,
                members: current.members.filter(
                  (member) => member.id !== memberId,
                ),
              },
      );
    },
  });
}
