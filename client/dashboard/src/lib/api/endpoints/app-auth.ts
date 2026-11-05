import { z } from 'zod';
import { emptyResponseSchema } from './schemas';
import { defineEndpoint } from './types';

const authorizedOriginSchema = z.looseObject({
  id: z.string(),
  service: z.string(),
  params: z.array(z.string()),
  created_at: z.string().optional(),
});

const oauthProviderSchema = z.looseObject({
  id: z.string(),
  provider_name: z.string(),
  created_at: z.string().optional(),
});

const oauthClientSchema = z.looseObject({
  id: z.string(),
  provider_id: z.string(),
  client_name: z.string(),
  client_id: z.string().nullable().optional(),
  extra_audiences: z.array(z.string()).nullable().optional(),
  discovery_endpoint: z.string().nullable().optional(),
  redirect_to: z.string().nullable().optional(),
  meta: z.unknown().optional(),
  use_shared_credentials: z.boolean().nullable().optional(),
  created_at: z.number().optional(),
});

// Postmark reports PascalCase fields; the dashboard reads only `Confirmed`.
const emailSenderVerificationSchema = z.looseObject({
  Confirmed: z.boolean().optional(),
});

/** P2.17: the Cloudflare Email Sending state for an app's sender domain. */
const senderDomainSchema = z.looseObject({
  domain: z.string().optional(),
  cloudflare_zone_id: z.string().nullable().optional(),
  cloudflare_enabled: z.boolean().optional(),
  ownership_verified: z.boolean().optional(),
  verified: z.boolean().optional(),
  last_error: z.string().nullable().optional(),
});

const senderVerificationSchema = z.looseObject({
  instant: z.looseObject({
    'verified?': z.boolean().nullable().optional(),
  }),
  verification: emailSenderVerificationSchema.nullable(),
  domain: senderDomainSchema.nullable().optional(),
});

const testUserSchema = z.looseObject({
  id: z.string(),
  app_id: z.string(),
  email: z.string(),
  code: z.string(),
  created_at: z.string(),
});

const oauthClientBody = {
  provider_id: z.string(),
  client_name: z.string(),
  client_id: z.string().optional(),
  client_secret: z.string().optional(),
  extra_audiences: z.array(z.string()).optional(),
  discovery_endpoint: z.string().optional(),
  redirect_to: z.string().optional(),
  meta: z.record(z.string(), z.unknown()).optional(),
  use_shared_credentials: z.boolean().optional(),
};

/** Authentication settings: providers, origins, email and test users (C.11). */
export const appAuthEndpoints = {
  get: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/auth',
    response: z.looseObject({
      authorized_redirect_origins: z
        .array(authorizedOriginSchema)
        .nullable()
        .optional(),
      oauth_service_providers: z
        .array(oauthProviderSchema)
        .nullable()
        .optional(),
      oauth_clients: z.array(oauthClientSchema).nullable().optional(),
      default_sender_email: z.string(),
    }),
  }),

  createProvider: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth_service_providers',
    body: z.looseObject({ provider_name: z.string() }),
    response: z.looseObject({ provider: oauthProviderSchema }),
  }),

  createClient: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth_clients',
    body: z.looseObject(oauthClientBody),
    response: z.looseObject({ client: oauthClientSchema }),
  }),

  updateClient: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/oauth_clients/:clientRecordId',
    body: z.looseObject({
      client_name: z.string().optional(),
      client_id: z.string().optional(),
      client_secret: z.string().optional(),
      extra_audiences: z.array(z.string()).optional(),
      discovery_endpoint: z.string().optional(),
      redirect_to: z.string().optional(),
      meta: z.record(z.string(), z.unknown()).optional(),
      use_shared_credentials: z.boolean().optional(),
    }),
    response: z.looseObject({ client: oauthClientSchema }),
  }),

  deleteClient: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/oauth_clients/:clientRecordId',
    response: emptyResponseSchema,
  }),

  addOrigin: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/authorized_redirect_origins',
    body: z.looseObject({ service: z.string(), params: z.array(z.string()) }),
    response: z.looseObject({ origin: authorizedOriginSchema }),
  }),

  deleteOrigin: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/authorized_redirect_origins/:originId',
    response: z.looseObject({ origin: authorizedOriginSchema }),
  }),

  defaultEmailTemplate: defineEndpoint({
    method: 'GET',
    path: '/dash/default-email-template',
    response: z.looseObject({
      'email-type': z.string(),
      'sender-email': z.string(),
      subject: z.string(),
      body: z.string(),
    }),
  }),

  saveEmailTemplate: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/email_templates',
    body: z.looseObject({
      'email-type': z.string(),
      subject: z.string(),
      body: z.string(),
      'sender-email': z.string().optional(),
      'sender-name': z.string().optional(),
    }),
    response: z.looseObject({ id: z.string() }),
  }),

  deleteEmailTemplate: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/email_templates/:templateId',
    response: emptyResponseSchema,
  }),

  sendTestEmail: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/send-test-email',
    body: z.looseObject({
      to: z.string(),
      subject: z.string(),
      body: z.string(),
      'sender-email': z.string().optional(),
      'sender-name': z.string().optional(),
    }),
    response: z.looseObject({ 'sent-to': z.string() }),
  }),

  getSenderVerification: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/sender-verification',
    response: senderVerificationSchema,
  }),

  sendSenderVerificationCode: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/sender-verification/send-magic-code',
    response: z.looseObject({ sent: z.boolean() }),
  }),

  verifySenderCode: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/sender-verification/verify-magic-code',
    body: z.looseObject({ code: z.string() }),
    response: z.looseObject({
      verified: z.boolean(),
      'cloudflare-enabled': z.boolean().optional(),
      error: z.string().nullable().optional(),
    }),
  }),

  listTestUsers: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/test_users',
    response: z.looseObject({ 'test-users': z.array(testUserSchema) }),
  }),

  addTestUser: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/test_users',
    body: z.looseObject({ email: z.string(), code: z.string() }),
    response: z.looseObject({ 'test-user': testUserSchema }),
  }),

  deleteTestUser: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/test_users',
    body: z.looseObject({ id: z.string() }),
    response: z.looseObject({ 'test-user': testUserSchema.nullable() }),
  }),
};

export type AuthorizedOrigin = z.output<typeof authorizedOriginSchema>;
export type OAuthProvider = z.output<typeof oauthProviderSchema>;
export type OAuthClient = z.output<typeof oauthClientSchema>;
export type AppAuthData = z.output<typeof appAuthEndpoints.get.response>;
export type SenderVerification = z.output<typeof senderVerificationSchema>;
export type TestUser = z.output<typeof testUserSchema>;
export type OAuthClientBody = z.input<
  NonNullable<typeof appAuthEndpoints.createClient.body>
>;
export type OAuthClientUpdateBody = z.input<
  NonNullable<typeof appAuthEndpoints.updateClient.body>
>;
