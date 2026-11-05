import { z } from 'zod';
import {
  emptyResponseSchema,
  memberRoleSchema,
  orgDetailsSchema,
  orgSchema,
} from './schemas';
import { defineEndpoint } from './types';

/** Workspace member and settings pages (C.17). */
export const orgEndpoints = {
  get: defineEndpoint({
    method: 'GET',
    path: '/dash/orgs/:orgId',
    response: orgDetailsSchema,
  }),

  create: defineEndpoint({
    method: 'POST',
    path: '/dash/orgs',
    body: z.looseObject({ title: z.string() }),
    response: z.looseObject({ org: orgSchema }),
  }),

  rename: defineEndpoint({
    method: 'POST',
    path: '/dash/orgs/:orgId/rename',
    body: z.looseObject({ title: z.string() }),
    response: emptyResponseSchema,
  }),

  remove: defineEndpoint({
    method: 'DELETE',
    path: '/dash/orgs/:orgId',
    response: z.looseObject({ ok: z.boolean() }),
  }),

  inviteMember: defineEndpoint({
    method: 'POST',
    path: '/dash/orgs/:orgId/invite/send',
    body: z.looseObject({
      'invitee-email': z.string(),
      role: memberRoleSchema,
    }),
    response: emptyResponseSchema,
  }),

  revokeInvite: defineEndpoint({
    method: 'DELETE',
    path: '/dash/orgs/:orgId/invite/revoke',
    body: z.looseObject({ 'invite-id': z.string() }),
    response: emptyResponseSchema,
  }),

  removeMember: defineEndpoint({
    method: 'DELETE',
    path: '/dash/orgs/:orgId/members/remove',
    body: z.looseObject({ id: z.string() }),
    response: emptyResponseSchema,
  }),

  updateMember: defineEndpoint({
    method: 'POST',
    path: '/dash/orgs/:orgId/members/update',
    body: z.looseObject({ id: z.string(), role: memberRoleSchema }),
    response: emptyResponseSchema,
  }),
};
