import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  appAuthEndpoints,
  appEndpoints,
  type AppAuthData,
  type AuthorizedOrigin,
  type OAuthClient,
  type OAuthClientBody,
  type OAuthClientUpdateBody,
  type TestUser,
} from '@/lib/api/endpoints';
import { overviewQueryKeys } from '@/lib/apps/overview-queries';
import { updateCachedApp } from '@/lib/apps/settings-queries';

/**
 * C.11 data: the app's providers, clients and redirect origins, plus the email
 * settings and test users. Mutations write the acknowledged row into the cached
 * payload so the cards, tables and sheet agree without a refetch racing the
 * next action.
 */

export const appAuthQueryKeys = {
  auth: (appId: string) => ['dash', 'apps', appId, 'auth'] as const,
  testUsers: (appId: string) =>
    ['dash', 'apps', appId, 'auth', 'test-users'] as const,
};

export function useAppAuth(appId: string) {
  return useQuery({
    queryKey: appAuthQueryKeys.auth(appId),
    queryFn: ({ signal }) =>
      apiFetch(appAuthEndpoints.get, { params: { appId }, signal }),
  });
}

function updateCachedAuth(
  queryClient: QueryClient,
  appId: string,
  update: (auth: AppAuthData) => AppAuthData,
): void {
  queryClient.setQueryData<AppAuthData>(
    appAuthQueryKeys.auth(appId),
    (current) => (current === undefined ? current : update(current)),
  );
}

export function useCreateProvider(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (providerName: string) =>
      apiFetch(appAuthEndpoints.createProvider, {
        params: { appId },
        body: { provider_name: providerName },
      }),
    onSuccess: (result) => {
      updateCachedAuth(queryClient, appId, (auth) => ({
        ...auth,
        oauth_service_providers: [
          result.provider,
          ...(auth.oauth_service_providers ?? []),
        ],
      }));
    },
  });
}

export interface CreateClientVariables {
  body: OAuthClientBody;
}

export function useCreateClient(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ body }: CreateClientVariables) =>
      apiFetch(appAuthEndpoints.createClient, { params: { appId }, body }),
    onSuccess: (result) => {
      updateCachedAuth(queryClient, appId, (auth) => ({
        ...auth,
        oauth_clients: [result.client, ...(auth.oauth_clients ?? [])],
      }));
    },
  });
}

export interface UpdateClientVariables {
  clientRecordId: string;
  body: OAuthClientUpdateBody;
}

export function useUpdateClient(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ clientRecordId, body }: UpdateClientVariables) =>
      apiFetch(appAuthEndpoints.updateClient, {
        params: { appId, clientRecordId },
        body,
      }),
    onSuccess: (result) => {
      updateCachedAuth(queryClient, appId, (auth) => ({
        ...auth,
        oauth_clients: (auth.oauth_clients ?? []).map((client) =>
          client.id === result.client.id ? result.client : client,
        ),
      }));
    },
  });
}

export function useDeleteClient(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (clientRecordId: string) =>
      apiFetch(appAuthEndpoints.deleteClient, {
        params: { appId, clientRecordId },
      }),
    onSuccess: (_result, clientRecordId) => {
      updateCachedAuth(queryClient, appId, (auth) => ({
        ...auth,
        oauth_clients: (auth.oauth_clients ?? []).filter(
          (client) => client.id !== clientRecordId,
        ),
      }));
    },
  });
}

export interface AddOriginVariables {
  service: string;
  params: string[];
}

export function useAddOrigin(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ service, params }: AddOriginVariables) =>
      apiFetch(appAuthEndpoints.addOrigin, {
        params: { appId },
        body: { service, params },
      }),
    onSuccess: (result) => {
      updateCachedAuth(queryClient, appId, (auth) => ({
        ...auth,
        authorized_redirect_origins: [
          result.origin,
          ...(auth.authorized_redirect_origins ?? []),
        ],
      }));
    },
  });
}

export function useDeleteOrigin(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (originId: string) =>
      apiFetch(appAuthEndpoints.deleteOrigin, {
        params: { appId, originId },
      }),
    onSuccess: (_result, originId) => {
      updateCachedAuth(queryClient, appId, (auth) => ({
        ...auth,
        authorized_redirect_origins: (
          auth.authorized_redirect_origins ?? []
        ).filter((origin) => origin.id !== originId),
      }));
    },
  });
}

export function clientsFromAuth(auth: AppAuthData | undefined): OAuthClient[] {
  return auth?.oauth_clients ?? [];
}

export function originsFromAuth(
  auth: AppAuthData | undefined,
): AuthorizedOrigin[] {
  return auth?.authorized_redirect_origins ?? [];
}

export interface SaveEmailTemplateVariables {
  subject: string;
  body: string;
  senderName: string;
  senderEmail: string;
}

/**
 * Saves the magic-code template. The server returns only the row id, so the
 * cached app gets the same values that were sent; the stored sender name falls
 * back to the app title when the field is blank.
 */
export function useSaveEmailTemplate(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      subject,
      body,
      senderName,
      senderEmail,
    }: SaveEmailTemplateVariables) =>
      apiFetch(appAuthEndpoints.saveEmailTemplate, {
        params: { appId },
        body: {
          'email-type': 'magic-code',
          subject,
          body,
          'sender-name':
            senderName.trim() === '' ? undefined : senderName.trim(),
          'sender-email':
            senderEmail.trim() === '' ? undefined : senderEmail.trim(),
        },
      }),
    onSuccess: (result, variables) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        magic_code_email_template: {
          id: result.id,
          name: variables.senderName.trim() || app.title,
          email: variables.senderEmail.trim() || null,
          subject: variables.subject.trim(),
          body: variables.body.trim(),
        },
      }));
    },
  });
}

/** Removes the stored template so the platform defaults apply again. */
export function useDeleteEmailTemplate(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (templateId: string) =>
      apiFetch(appAuthEndpoints.deleteEmailTemplate, {
        params: { appId, templateId },
      }),
    onSuccess: () => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        magic_code_email_template: null,
      }));
    },
  });
}

/** Sender verification is admin-only and shared with the overview health check. */
export function useSendSenderVerificationCode(appId: string) {
  return useMutation({
    mutationFn: () =>
      apiFetch(appAuthEndpoints.sendSenderVerificationCode, {
        params: { appId },
      }),
  });
}

export interface SendTestEmailVariables {
  to: string;
  subject: string;
  body: string;
  senderName: string;
  senderEmail: string;
}

export function useSendTestEmail(appId: string) {
  return useMutation({
    mutationFn: ({
      to,
      subject,
      body,
      senderName,
      senderEmail,
    }: SendTestEmailVariables) =>
      apiFetch(appAuthEndpoints.sendTestEmail, {
        params: { appId },
        body: {
          to,
          subject,
          body,
          'sender-name':
            senderName.trim() === '' ? undefined : senderName.trim(),
          'sender-email':
            senderEmail.trim() === '' ? undefined : senderEmail.trim(),
        },
      }),
  });
}

export function useVerifySenderCode(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (code: string) =>
      apiFetch(appAuthEndpoints.verifySenderCode, {
        params: { appId },
        body: { code },
      }),
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: overviewQueryKeys.senderVerification(appId),
      });
    },
  });
}

export function useTestUsers(appId: string) {
  return useQuery({
    queryKey: appAuthQueryKeys.testUsers(appId),
    queryFn: ({ signal }) =>
      apiFetch(appAuthEndpoints.listTestUsers, { params: { appId }, signal }),
  });
}

export interface AddTestUserVariables {
  email: string;
  code: string;
}

export function useAddTestUser(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ email, code }: AddTestUserVariables) =>
      apiFetch(appAuthEndpoints.addTestUser, {
        params: { appId },
        body: { email, code },
      }),
    onSuccess: (result) => {
      queryClient.setQueryData<{ 'test-users': TestUser[] }>(
        appAuthQueryKeys.testUsers(appId),
        (current) => ({
          'test-users': [
            result['test-user'],
            ...(current?.['test-users'] ?? []).filter(
              (user) => user.id !== result['test-user'].id,
            ),
          ],
        }),
      );
    },
  });
}

export function useDeleteTestUser(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) =>
      apiFetch(appAuthEndpoints.deleteTestUser, {
        params: { appId },
        body: { id },
      }),
    onSuccess: (_result, id) => {
      queryClient.setQueryData<{ 'test-users': TestUser[] }>(
        appAuthQueryKeys.testUsers(appId),
        (current) => ({
          'test-users': (current?.['test-users'] ?? []).filter(
            (user) => user.id !== id,
          ),
        }),
      );
    },
  });
}

export function testUsersFrom(
  data: { 'test-users': TestUser[] } | undefined,
): TestUser[] {
  return data?.['test-users'] ?? [];
}

/** P2.4: the server clamps the value to 1–60 minutes. */
export function useSetMagicCodeExpiry(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (expiry: number) =>
      apiFetch(appEndpoints.setMagicCodeExpiry, {
        params: { appId },
        body: { expiry },
      }),
    onSuccess: (result, expiry) => {
      updateCachedApp(queryClient, appId, (app) => ({
        ...app,
        magic_code_expiry_minutes:
          result.app.magic_code_expiry_minutes ?? expiry,
      }));
    },
  });
}
