import { z } from 'zod';
import { defineEndpoint } from './types';

/**
 * Consent screen claim (C.2). Deny and grant are browser form posts, not
 * fetches, so they are not declared here.
 */
export const platformOauthEndpoints = {
  claim: defineEndpoint({
    method: 'POST',
    path: '/platform/oauth/claim',
    body: z.looseObject({ redirect: z.string() }),
    response: z.looseObject({
      appName: z.string(),
      appLogo: z.string().nullable(),
      scopes: z.array(z.string()),
      redirectOrigin: z.string(),
      userEmail: z.string(),
      grantToken: z.string(),
    }),
  }),
};
