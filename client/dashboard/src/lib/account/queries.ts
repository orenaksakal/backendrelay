import {
  queryOptions,
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  accountEndpoints,
  type AuthorizedOauthApp,
  type PersonalAccessToken,
} from '@/lib/api/endpoints';
import type { Dash } from '@/lib/api/endpoints/schemas';
import { workspaceQueryKeys } from '@/lib/workspace/queries';

/** Account data is separate from the bootstrap payload (C.18). */
export const accountQueryKeys = {
  tokens: ['dash', 'personal_access_tokens'] as const,
  authorizedApps: ['dash', 'user', 'oauth_apps'] as const,
};

export interface PersonalTokenList {
  data: PersonalAccessToken[];
}

export interface AuthorizedAppList {
  oauthApps: AuthorizedOauthApp[];
}

export function personalTokensQueryOptions() {
  return queryOptions({
    queryKey: accountQueryKeys.tokens,
    queryFn: ({ signal }) =>
      apiFetch(accountEndpoints.listPersonalTokens, { signal }),
  });
}

export function authorizedAppsQueryOptions() {
  return queryOptions({
    queryKey: accountQueryKeys.authorizedApps,
    queryFn: ({ signal }) =>
      apiFetch(accountEndpoints.listAuthorizedApps, { signal }),
  });
}

export function usePersonalTokens() {
  return useQuery(personalTokensQueryOptions());
}

export function useAuthorizedApps() {
  return useQuery(authorizedAppsQueryOptions());
}

/**
 * Creates a token and inserts it into the cached list, so the new row is
 * present without a refetch that could race the one-time token dialog.
 */
export function useCreatePersonalToken() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (name: string) =>
      apiFetch(accountEndpoints.createPersonalToken, { body: { name } }),
    onSuccess: (result) => {
      // Keep the secret out of the list cache; the dialog shows it once.
      const tokenRow: PersonalAccessToken = {
        id: result.data.id,
        user_id: result.data.user_id,
        name: result.data.name,
        created_at: result.data.created_at,
        last_four: result.data.last_four,
        lastFour: result.data.lastFour,
        token_last_four: result.data.token_last_four,
      };
      queryClient.setQueryData<PersonalTokenList>(
        accountQueryKeys.tokens,
        (current) => {
          if (current === undefined) return current;
          return { ...current, data: [tokenRow, ...current.data] };
        },
      );
    },
  });
}

export function useDeletePersonalToken() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (tokenId: string) =>
      apiFetch(accountEndpoints.deletePersonalToken, { params: { tokenId } }),
    onSuccess: (_result, tokenId) => {
      queryClient.setQueryData<PersonalTokenList>(
        accountQueryKeys.tokens,
        (current) => {
          if (current === undefined) return current;
          return {
            ...current,
            data: current.data.filter((token) => token.id !== tokenId),
          };
        },
      );
    },
  });
}

/** Revoking returns the remaining authorized apps, which replaces the list. */
export function useRevokeAuthorizedApp() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (oauthAppId: string) =>
      apiFetch(accountEndpoints.revokeAuthorizedApp, {
        body: { oauthAppId },
      }),
    onSuccess: (result) => {
      queryClient.setQueryData<AuthorizedAppList>(
        accountQueryKeys.authorizedApps,
        result,
      );
    },
  });
}

export function useAcceptInvite() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (inviteId: string) =>
      apiFetch(accountEndpoints.acceptInvite, {
        body: { 'invite-id': inviteId },
      }),
    onSuccess: (_result, inviteId) => {
      removeBootstrapInvite(queryClient, inviteId);
    },
  });
}

export function useDeclineInvite() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (inviteId: string) =>
      apiFetch(accountEndpoints.declineInvite, {
        body: { 'invite-id': inviteId },
      }),
    onSuccess: (_result, inviteId) => {
      removeBootstrapInvite(queryClient, inviteId);
    },
  });
}

/**
 * Removes the invite from the bootstrap cache locally. A refetch would return
 * the server's page, but the list is already correct and the operator should
 * not wait for a second round trip after accepting or declining.
 */
function removeBootstrapInvite(
  queryClient: ReturnType<typeof useQueryClient>,
  inviteId: string,
): void {
  queryClient.setQueryData<Dash>(workspaceQueryKeys.bootstrap, (current) => {
    if (current === undefined) return current;
    return {
      ...current,
      invites: current.invites?.filter((invite) => invite.id !== inviteId),
    };
  });
}
