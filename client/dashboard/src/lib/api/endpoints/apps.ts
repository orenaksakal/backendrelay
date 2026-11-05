import { z } from 'zod';
import {
  appSchema,
  appStatusSchema,
  emptyResponseSchema,
  memberRoleSchema,
} from './schemas';
import { defineEndpoint } from './types';

const ruleRecordSchema = z.looseObject({
  app_id: z.string(),
  code: z.unknown(),
  version: z.number(),
});

const indexingJobSchema = z.looseObject({
  id: z.string(),
  app_id: z.string(),
  group_id: z.string().nullable().optional(),
  attr_id: z.string(),
  attr_name: z.string(),
  job_type: z.string(),
  job_status: z.string(),
  job_stage: z.string().nullable().optional(),
  work_estimate: z.number().nullable().optional(),
  work_completed: z.number().nullable().optional(),
  error: z.string().nullable().optional(),
  checked_data_type: z.string().nullable().optional(),
  created_at: z.string(),
  updated_at: z.string(),
  done_at: z.string().nullable().optional(),
  invalid_unique_value: z.unknown().optional(),
  invalid_triples_sample: z.unknown().optional(),
  error_data: z.unknown().optional(),
});

/** C.7: one constraint check the server runs in the background. */
export type IndexingJob = z.infer<typeof indexingJobSchema>;

export const indexingJobTypeSchema = z.enum([
  'check-data-type',
  'remove-data-type',
  'index',
  'remove-index',
  'unique',
  'remove-unique',
  'required',
  'remove-required',
]);

export type IndexingJobType = z.infer<typeof indexingJobTypeSchema>;

/** C.7 Deleted tab: a soft-deleted attribute and its deletion time. */
export const softDeletedAttributeSchema = z.looseObject({
  id: z.string(),
  'forward-identity': z.tuple([z.string(), z.string(), z.string()]),
  'reverse-identity': z.tuple([z.string(), z.string(), z.string()]).optional(),
  'value-type': z.string(),
  cardinality: z.string(),
  'index?': z.boolean().optional(),
  'unique?': z.boolean().optional(),
  'required?': z.boolean().optional(),
  'checked-data-type': z.string().optional(),
  'on-delete': z.string().optional(),
  'on-delete-reverse': z.string().optional(),
  'deletion-marked-at': z.string().optional(),
});

export type SoftDeletedAttribute = z.infer<typeof softDeletedAttributeSchema>;

const appUsageSchema = z.looseObject({
  appBytes: z.number(),
  storageBytes: z.number(),
  fileCount: z.number().nullable(),
  liveSessions: z.number(),
  transactions: z.looseObject({
    last24h: z.number(),
    last7d: z.number(),
    daily: z.array(z.looseObject({ date: z.string(), count: z.number() })),
  }),
});

const appSummarySchema = z.looseObject({
  id: z.string(),
  liveSessions: z.number(),
  appBytes: z.number(),
  storageBytes: z.number(),
  lastWriteAt: z.string().nullable(),
  lastBackupAt: z.string().nullable(),
});

export type AppSummary = z.infer<typeof appSummarySchema>;

/** C.10/D7.2: the collaborator-readable schema behind the visual editor. */
const adminAttributeSchema = z.looseObject({
  id: z.string(),
  'forward-identity': z.tuple([z.string(), z.string(), z.string()]),
  'reverse-identity': z.tuple([z.string(), z.string(), z.string()]).optional(),
  'index?': z.boolean().optional(),
  'unique?': z.boolean().optional(),
  'required?': z.boolean().optional(),
  cardinality: z.enum(['one', 'many']),
  'value-type': z.enum(['ref', 'blob']),
  'inferred-types': z.array(z.string()).nullable().optional(),
  catalog: z.enum(['user', 'system']).optional(),
  'checked-data-type': z
    .enum(['string', 'number', 'boolean', 'date'])
    .optional(),
  'on-delete': z.string().optional(),
  'on-delete-reverse': z.string().optional(),
});

/** App screens: overview, explorer, schema, settings and team (C.5–C.7, C.16). */
export const appEndpoints = {
  get: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId',
    response: z.looseObject({ app: appSchema }),
  }),

  create: defineEndpoint({
    method: 'POST',
    path: '/dash/apps',
    body: z.looseObject({
      id: z.string(),
      admin_token: z.string(),
      title: z.string(),
      org_id: z.string().optional(),
    }),
    response: z.looseObject({ app: appSchema }),
  }),

  rename: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/rename',
    body: z.looseObject({ title: z.string() }),
    response: emptyResponseSchema,
  }),

  setStatus: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/status',
    body: z.looseObject({ status: appStatusSchema }),
    response: z.looseObject({ status: appStatusSchema }),
  }),

  transferToOrg: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/transfer_to_org/:orgId',
    response: z.looseObject({}),
  }),

  clear: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/clear',
    response: z.looseObject({ ok: z.boolean() }),
  }),

  remove: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId',
    response: z.looseObject({ ok: z.boolean() }),
  }),

  stats: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/stats',
    response: z.looseObject({
      count: z.number(),
      origins: z.record(z.string(), z.number()),
    }),
  }),

  usage: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/usage',
    response: appUsageSchema,
  }),

  /** One query per metric for every visible app in a workspace (D0.2). */
  summary: defineEndpoint({
    method: 'GET',
    path: '/dash/apps-summary',
    response: z.looseObject({ apps: z.array(appSummarySchema) }),
  }),

  setMagicCodeExpiry: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/set-magic-code-expiry',
    body: z.looseObject({ expiry: z.number() }),
    response: z.looseObject({ app: appSchema }),
  }),

  setAdminToken: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/tokens',
    body: z.looseObject({ 'admin-token': z.string() }),
    response: z.looseObject({
      token: z.string(),
      app_id: z.string(),
      created_at: z.string(),
    }),
  }),

  /** Server-generated rotation; the old token stops working immediately (D0.6). */
  rotateAdminToken: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/tokens/rotate',
    response: z.looseObject({ adminToken: z.string() }),
  }),

  softDeletedAttributes: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/soft_deleted_attrs',
    response: z.looseObject({
      attrs: z.array(softDeletedAttributeSchema),
      'grace-period-days': z.number(),
    }),
  }),

  saveRules: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/rules',
    body: z.looseObject({ code: z.unknown() }),
    response: z.looseObject({ rules: ruleRecordSchema.nullable() }),
  }),

  validateRules: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/rules/validate',
    body: z.looseObject({ code: z.unknown() }),
    response: z.union([
      z.looseObject({ ok: z.literal(true) }),
      z.looseObject({
        ok: z.literal(false),
        errors: z.array(
          z.looseObject({ in: z.array(z.string()), message: z.string() }),
        ),
      }),
    ]),
  }),

  ruleVersions: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/rule-versions',
    response: z.looseObject({
      versions: z.array(
        z.looseObject({
          version: z.number(),
          edits: z.array(z.unknown()),
          created_at: z.string(),
        }),
      ),
    }),
  }),

  /** Attributes for collaborators, who cannot open the admin-token client. */
  schemaPull: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/schema/pull',
    response: z.looseObject({
      attrs: z.array(adminAttributeSchema),
      'app-title': z.string().optional(),
    }),
  }),

  indexingJob: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/indexing-jobs/:jobId',
    response: z.looseObject({ job: indexingJobSchema.nullable() }),
  }),

  indexingJobGroup: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/indexing-jobs/group/:groupId',
    response: z.looseObject({ jobs: z.array(indexingJobSchema) }),
  }),

  /** Starts a constraint check; C.7 polls the returned job once a second. */
  createIndexingJob: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/indexing-jobs',
    body: z.looseObject({
      'attr-id': z.string(),
      'job-type': indexingJobTypeSchema,
      'checked-data-type': z.string().optional(),
    }),
    response: z.looseObject({ job: indexingJobSchema }),
  }),

  inviteMember: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/invite/send',
    body: z.looseObject({
      'invitee-email': z.string(),
      role: memberRoleSchema,
    }),
    response: emptyResponseSchema,
  }),

  revokeInvite: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/invite/revoke',
    body: z.looseObject({ 'invite-id': z.string() }),
    response: emptyResponseSchema,
  }),

  removeMember: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/members/remove',
    body: z.looseObject({ id: z.string() }),
    response: emptyResponseSchema,
  }),

  updateMember: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/members/update',
    body: z.looseObject({ id: z.string(), role: memberRoleSchema }),
    response: emptyResponseSchema,
  }),
};
