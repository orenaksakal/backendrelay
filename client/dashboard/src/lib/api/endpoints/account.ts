import { z } from 'zod';
import { emptyResponseSchema } from './schemas';
import { defineEndpoint } from './types';

const personalAccessTokenSchema = z.looseObject({
  id: z.string(),
  user_id: z.string(),
  name: z.string(),
  /** The server sends an ISO timestamp; older builds sent epoch milliseconds. */
  created_at: z.union([z.string(), z.number()]),
  /**
   * The list endpoint only knows a hashed lookup key, so the last four
   * characters are available only for tokens created by a server that stores
   * them. The known spellings are accepted while the server field is settled.
   */
  last_four: z.string().nullable().optional(),
  lastFour: z.string().nullable().optional(),
  token_last_four: z.string().nullable().optional(),
});

const authorizedOauthAppSchema = z.looseObject({
  id: z.string(),
  name: z.string(),
  logo: z.string().nullable(),
  homePage: z.string().nullable(),
  privacyPolicyLink: z.string().nullable(),
  tosLink: z.string().nullable(),
  /** Optional until the server returns them (C.18). */
  scopes: z.array(z.string()).optional(),
  grantedScopes: z.array(z.string()).optional(),
  granted_scopes: z.array(z.string()).optional(),
  authorizedAt: z.string().optional(),
  authorized_at: z.string().optional(),
  createdAt: z.string().optional(),
  created_at: z.string().optional(),
});

export type PersonalAccessToken = z.output<typeof personalAccessTokenSchema>;
export type CreatedPersonalAccessToken = PersonalAccessToken & {
  token: string;
};
export type AuthorizedOauthApp = z.output<typeof authorizedOauthAppSchema>;

/** Account pages: tokens, authorized apps and invites (C.18). */
export const accountEndpoints = {
  listPersonalTokens: defineEndpoint({
    method: 'GET',
    path: '/dash/personal_access_tokens',
    response: z.looseObject({
      data: z.array(personalAccessTokenSchema),
    }),
  }),

  createPersonalToken: defineEndpoint({
    method: 'POST',
    path: '/dash/personal_access_tokens',
    body: z.looseObject({ name: z.string() }),
    response: z.looseObject({
      data: personalAccessTokenSchema.extend({ token: z.string() }),
    }),
  }),

  deletePersonalToken: defineEndpoint({
    method: 'DELETE',
    path: '/dash/personal_access_tokens/:tokenId',
    response: emptyResponseSchema,
  }),

  listAuthorizedApps: defineEndpoint({
    method: 'GET',
    path: '/dash/user/oauth_apps',
    response: z.looseObject({
      oauthApps: z.array(authorizedOauthAppSchema),
    }),
  }),

  revokeAuthorizedApp: defineEndpoint({
    method: 'POST',
    path: '/dash/user/oauth_apps/revoke_access',
    body: z.looseObject({ oauthAppId: z.string() }),
    response: z.looseObject({
      oauthApps: z.array(authorizedOauthAppSchema),
    }),
  }),

  acceptInvite: defineEndpoint({
    method: 'POST',
    path: '/dash/invites/accept',
    body: z.looseObject({ 'invite-id': z.string() }),
    response: emptyResponseSchema,
  }),

  declineInvite: defineEndpoint({
    method: 'POST',
    path: '/dash/invites/decline',
    body: z.looseObject({ 'invite-id': z.string() }),
    response: emptyResponseSchema,
  }),
};
