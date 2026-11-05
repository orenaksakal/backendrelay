import {
  type QueryClient,
  queryOptions,
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  oauthAppEndpoints,
  type OauthApp,
  type OauthAppClient,
  type OauthAppClientSecret,
} from '@/lib/api/endpoints';
import type { OauthAppUpdateBody } from './model';

/**
 * C.14 data. One list request carries every OAuth app with its clients and
 * client secrets, so every mutation writes the acknowledged rows back into that
 * cache instead of refetching and racing the one-time secret dialog.
 */

export const oauthAppQueryKeys = {
  list: (appId: string) => ['dash', 'apps', appId, 'oauth-apps'] as const,
};

export interface OauthAppList {
  apps: OauthApp[];
}

export function oauthAppsQueryOptions(appId: string) {
  return queryOptions({
    queryKey: oauthAppQueryKeys.list(appId),
    queryFn: ({ signal }) =>
      apiFetch(oauthAppEndpoints.list, { params: { appId }, signal }),
    // Mutations write acknowledged rows straight into this cache; keeping the
    // list fresh for the session avoids a refetch dropping a just-created app.
    staleTime: 30_000,
  });
}

export function useOauthApps(appId: string, enabled = true) {
  return useQuery({ ...oauthAppsQueryOptions(appId), enabled });
}

function updateApps(
  queryClient: QueryClient,
  appId: string,
  update: (apps: OauthApp[]) => OauthApp[],
): void {
  queryClient.setQueryData<OauthAppList>(
    oauthAppQueryKeys.list(appId),
    (current) =>
      current === undefined ? current : { apps: update(current.apps) },
  );
}

function updateOauthApp(
  queryClient: QueryClient,
  appId: string,
  oauthAppId: string,
  update: (app: OauthApp) => OauthApp,
): void {
  updateApps(queryClient, appId, (apps) =>
    apps.map((app) => (app.id === oauthAppId ? update(app) : app)),
  );
}

export interface CreateOauthAppVariables {
  name: string;
  logo: string | null;
  homePage: string | null;
  privacyPolicy: string | null;
  terms: string | null;
  supportEmail: string | null;
}

export function useCreateOauthApp(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (variables: CreateOauthAppVariables) =>
      apiFetch(oauthAppEndpoints.create, {
        params: { appId },
        body: {
          app_id: appId,
          app_name: variables.name,
          app_logo: variables.logo,
          app_home_page: variables.homePage,
          app_privacy_policy_link: variables.privacyPolicy,
          app_tos_link: variables.terms,
          support_email: variables.supportEmail,
        },
      }),
    onSuccess: (result) => {
      const created: OauthApp = {
        ...result.app,
        clients: result.app.clients ?? [],
      };
      updateApps(queryClient, appId, (apps) => [created, ...apps]);
    },
  });
}

export interface UpdateOauthAppVariables {
  oauthAppId: string;
  body: OauthAppUpdateBody;
}

export function useUpdateOauthApp(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ oauthAppId, body }: UpdateOauthAppVariables) =>
      apiFetch(oauthAppEndpoints.update, {
        params: { appId, oauthAppId },
        body,
      }),
    onSuccess: (result, { oauthAppId }) => {
      updateOauthApp(queryClient, appId, oauthAppId, (app) => ({
        ...app,
        ...result.app,
        clients: app.clients,
      }));
    },
  });
}

export function useDeleteOauthApp(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (oauthAppId: string) =>
      apiFetch(oauthAppEndpoints.remove, { params: { appId, oauthAppId } }),
    onSuccess: (_result, oauthAppId) => {
      updateApps(queryClient, appId, (apps) =>
        apps.filter((app) => app.id !== oauthAppId),
      );
    },
  });
}

export interface CreateOauthClientVariables {
  oauthAppId: string;
  name: string;
  redirectUrls: string[];
}

export function useCreateOauthClient(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      oauthAppId,
      name,
      redirectUrls,
    }: CreateOauthClientVariables) =>
      apiFetch(oauthAppEndpoints.createClient, {
        params: { appId, oauthAppId },
        body: {
          client_name: name,
          authorized_redirect_urls: redirectUrls,
        },
      }),
    onSuccess: (result, { oauthAppId }) => {
      const created: OauthAppClient = {
        ...result.client,
        clientSecrets: [result.clientSecret],
      };
      updateOauthApp(queryClient, appId, oauthAppId, (app) => ({
        ...app,
        clients: [created, ...(app.clients ?? [])],
      }));
    },
  });
}

export interface UpdateOauthClientVariables {
  oauthAppId: string;
  client: OauthAppClient;
  name: string;
  redirectUrls: string[];
}

/**
 * The server edits one redirect URL per request, so the sheet's list diff is
 * applied as a short sequence: rename, remove, then add.
 */
export function useUpdateOauthClient(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({
      client,
      name,
      redirectUrls,
    }: UpdateOauthClientVariables) => {
      const params = { appId, clientId: client.clientId };
      let latest = client;
      if (name !== client.clientName) {
        const result = await apiFetch(oauthAppEndpoints.updateClient, {
          params,
          body: { app_id: appId, client_name: name },
        });
        latest = result.client;
      }
      for (const url of client.authorizedRedirectUrls) {
        if (redirectUrls.includes(url)) continue;
        const result = await apiFetch(oauthAppEndpoints.updateClient, {
          params,
          body: { app_id: appId, remove_redirect_url: url },
        });
        latest = result.client;
      }
      for (const url of redirectUrls) {
        if (client.authorizedRedirectUrls.includes(url)) continue;
        const result = await apiFetch(oauthAppEndpoints.updateClient, {
          params,
          body: { app_id: appId, add_redirect_url: url },
        });
        latest = result.client;
      }
      return latest;
    },
    onSuccess: (client, { oauthAppId }) => {
      updateOauthApp(queryClient, appId, oauthAppId, (app) => ({
        ...app,
        clients: (app.clients ?? []).map((current) =>
          current.clientId === client.clientId
            ? { ...client, clientSecrets: current.clientSecrets }
            : current,
        ),
      }));
    },
    // A failure halfway through the sequence leaves the cache behind.
    onError: () => {
      void queryClient.invalidateQueries({
        queryKey: oauthAppQueryKeys.list(appId),
      });
    },
  });
}

export interface DeleteOauthClientVariables {
  oauthAppId: string;
  clientId: string;
}

/** Deleting a client also deletes the tokens it issued (server-side). */
export function useDeleteOauthClient(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ clientId }: DeleteOauthClientVariables) =>
      apiFetch(oauthAppEndpoints.removeClient, { params: { appId, clientId } }),
    onSuccess: (_result, { oauthAppId, clientId }) => {
      updateOauthApp(queryClient, appId, oauthAppId, (app) => ({
        ...app,
        clients: (app.clients ?? []).filter(
          (client) => client.clientId !== clientId,
        ),
      }));
    },
  });
}

export interface CreateClientSecretVariables {
  oauthAppId: string;
  clientId: string;
}

export function useCreateClientSecret(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ clientId }: CreateClientSecretVariables) =>
      apiFetch(oauthAppEndpoints.createClientSecret, {
        params: { appId, clientId },
      }),
    onSuccess: (result, { oauthAppId, clientId }) => {
      const secret: OauthAppClientSecret = result.clientSecret;
      updateOauthApp(queryClient, appId, oauthAppId, (app) => ({
        ...app,
        clients: (app.clients ?? []).map((client) =>
          client.clientId === clientId
            ? {
                ...client,
                clientSecrets: [secret, ...(client.clientSecrets ?? [])],
              }
            : client,
        ),
      }));
    },
  });
}

export interface DeleteClientSecretVariables {
  oauthAppId: string;
  clientId: string;
  clientSecretId: string;
}

export function useDeleteClientSecret(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ clientSecretId }: DeleteClientSecretVariables) =>
      apiFetch(oauthAppEndpoints.removeClientSecret, {
        params: { appId, clientSecretId },
      }),
    onSuccess: (_result, { oauthAppId, clientId, clientSecretId }) => {
      updateOauthApp(queryClient, appId, oauthAppId, (app) => ({
        ...app,
        clients: (app.clients ?? []).map((client) =>
          client.clientId === clientId
            ? {
                ...client,
                clientSecrets: (client.clientSecrets ?? []).filter(
                  (secret) => secret.id !== clientSecretId,
                ),
              }
            : client,
        ),
      }));
    },
  });
}
