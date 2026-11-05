import { z } from 'zod';
import { defineEndpoint } from './types';

export const oauthAppClientSecretSchema = z.looseObject({
  id: z.string(),
  clientId: z.string(),
  firstFour: z.string(),
  createdAt: z.string(),
});

export type OauthAppClientSecret = z.infer<typeof oauthAppClientSecretSchema>;

export const oauthAppClientSchema = z.looseObject({
  clientId: z.string(),
  oauthAppId: z.string(),
  clientName: z.string(),
  authorizedRedirectUrls: z.array(z.string()),
  clientSecrets: z.array(oauthAppClientSecretSchema).nullable().optional(),
  createdAt: z.string(),
  updatedAt: z.string(),
});

export type OauthAppClient = z.infer<typeof oauthAppClientSchema>;

export const oauthAppSchema = z.looseObject({
  id: z.string(),
  appId: z.string(),
  appName: z.string(),
  appLogo: z.string().nullable(),
  grantedScopes: z.array(z.string()),
  isPublic: z.boolean(),
  supportEmail: z.string().nullable(),
  appHomePage: z.string().nullable(),
  appPrivacyPolicyLink: z.string().nullable(),
  appTosLink: z.string().nullable(),
  createdAt: z.string(),
  updatedAt: z.string(),
  clients: z.array(oauthAppClientSchema).nullable().optional(),
});

export type OauthApp = z.infer<typeof oauthAppSchema>;

const oauthAppBody = {
  app_id: z.string(),
  app_name: z.string(),
  app_home_page: z.string().nullable(),
  app_logo: z.string().nullable(),
  app_privacy_policy_link: z.string().nullable(),
  app_tos_link: z.string().nullable(),
  support_email: z.string().nullable(),
};

/** Publishing this app as an OAuth provider (C.14). */
export const oauthAppEndpoints = {
  list: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/oauth-apps',
    response: z.looseObject({ apps: z.array(oauthAppSchema) }),
  }),

  create: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth-apps',
    body: z.looseObject(oauthAppBody),
    response: z.looseObject({ app: oauthAppSchema }),
  }),

  update: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth-apps/:oauthAppId',
    body: z.looseObject({
      app_id: z.string().optional(),
      app_name: z.string().optional(),
      app_home_page: z.string().nullable().optional(),
      app_logo: z.string().nullable().optional(),
      app_privacy_policy_link: z.string().nullable().optional(),
      app_tos_link: z.string().nullable().optional(),
      support_email: z.string().nullable().optional(),
    }),
    response: z.looseObject({ app: oauthAppSchema }),
  }),

  remove: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/oauth-apps/:oauthAppId',
    response: z.looseObject({ app: oauthAppSchema.optional() }),
  }),

  createClient: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth-apps/:oauthAppId/clients',
    body: z.looseObject({
      client_name: z.string(),
      authorized_redirect_urls: z.array(z.string()),
    }),
    response: z.looseObject({
      client: oauthAppClientSchema,
      /** The server creates the first secret with the client. */
      clientSecret: oauthAppClientSecretSchema,
      secretValue: z.string(),
    }),
  }),

  updateClient: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth-app-clients/:clientId',
    body: z.looseObject({
      app_id: z.string().optional(),
      client_name: z.string().nullable().optional(),
      add_redirect_url: z.string().nullable().optional(),
      remove_redirect_url: z.string().nullable().optional(),
    }),
    response: z.looseObject({ client: oauthAppClientSchema }),
  }),

  removeClient: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/oauth-app-clients/:clientId',
    response: z.looseObject({ client: oauthAppClientSchema.optional() }),
  }),

  createClientSecret: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth-app-clients/:clientId/client-secrets',
    response: z.looseObject({
      clientSecret: oauthAppClientSecretSchema,
      secretValue: z.string(),
    }),
  }),

  removeClientSecret: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/oauth-app-client-secrets/:clientSecretId',
    response: z.looseObject({
      clientSecret: oauthAppClientSecretSchema.optional(),
    }),
  }),
};
