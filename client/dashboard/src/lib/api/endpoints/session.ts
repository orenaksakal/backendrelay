import { z } from 'zod';
import {
  appAuthResultSchema,
  dashSchema,
  emptyResponseSchema,
  profileSchema,
  userSchema,
} from './schemas';
import { defineEndpoint } from './types';

/** Sign-in, the bootstrap payload and CLI approval (C.1). */
export const sessionEndpoints = {
  publicConfig: defineEndpoint({
    method: 'GET',
    path: '/dash/config',
    response: z.looseObject({
      version: z.string(),
      googleLogin: z.boolean(),
      signup: z.enum(['open', 'restricted', 'closed']),
      ephemeralApps: z.boolean(),
      private: z.boolean(),
    }),
  }),

  bootstrap: defineEndpoint({
    method: 'GET',
    path: '/dash',
    response: dashSchema,
  }),

  me: defineEndpoint({
    method: 'GET',
    path: '/dash/me',
    response: z.looseObject({ user: userSchema }),
  }),

  sendMagicCode: defineEndpoint({
    method: 'POST',
    path: '/dash/auth/send_magic_code',
    body: z.looseObject({ email: z.string() }),
    response: z.looseObject({ sent: z.boolean() }),
  }),

  verifyMagicCode: defineEndpoint({
    method: 'POST',
    path: '/dash/auth/verify_magic_code',
    body: z.looseObject({ email: z.string(), code: z.string() }),
    response: appAuthResultSchema,
  }),

  exchangeOAuthCode: defineEndpoint({
    method: 'POST',
    path: '/dash/oauth/token',
    body: z.looseObject({ code: z.string() }),
    response: appAuthResultSchema,
  }),

  signOut: defineEndpoint({
    method: 'POST',
    path: '/dash/signout',
    body: emptyResponseSchema,
    response: emptyResponseSchema,
  }),

  claimCliTicket: defineEndpoint({
    method: 'POST',
    path: '/dash/cli/auth/claim',
    body: z.looseObject({ ticket: z.string() }),
    response: z.looseObject({ ticket: z.string() }),
  }),

  voidCliTicket: defineEndpoint({
    method: 'POST',
    path: '/dash/cli/auth/void',
    body: z.looseObject({ ticket: z.string() }),
    response: emptyResponseSchema,
  }),

  updateProfile: defineEndpoint({
    method: 'POST',
    path: '/dash/profiles',
    body: z.looseObject({ meta: z.record(z.string(), z.unknown()) }),
    response: z.looseObject({ profile: profileSchema }),
  }),
};
