import { z } from 'zod';

/**
 * Shared wire schemas for the dashboard API. Objects are loose: the server
 * sends more fields than each screen reads, and new fields must not break an
 * older dashboard build.
 */

export const emptyResponseSchema = z.looseObject({});

export const appStatusSchema = z.enum(['active', 'read-only', 'disabled']);

export const memberRoleSchema = z.enum(['owner', 'admin', 'collaborator']);

export const effectiveRoleSchema = z.enum([
  'owner',
  'admin',
  'collaborator',
  'app-member',
]);

export const userSchema = z.looseObject({
  id: z.string(),
  email: z.string(),
});

export const profileSchema = z.looseObject({
  id: z.string(),
  meta: z.unknown(),
  created_at: z.string(),
});

export const appMemberSchema = z.looseObject({
  id: z.string(),
  email: z.string(),
  role: memberRoleSchema,
  /** Org members expose their join time; app members do not. */
  created_at: z.string().optional(),
});

export const appInviteSchema = z.looseObject({
  id: z.string(),
  email: z.string(),
  status: z.enum(['pending', 'accepted', 'revoked']),
  role: memberRoleSchema,
  sent_at: z.string(),
  expired: z.boolean(),
});

export const emailTemplateSchema = z.looseObject({
  id: z.string(),
  name: z.string(),
  email: z.string().nullable().optional(),
  subject: z.string(),
  body: z.string(),
});

export const webhookActionSchema = z.enum(['create', 'update', 'delete']);

export const webhookSchema = z.looseObject({
  id: z.string(),
  sink: z.looseObject({ url: z.string() }),
  namespaces: z.array(z.string()).nullable().optional(),
  actions: z.array(webhookActionSchema),
  status: z.enum(['active', 'disabled']),
  disabled_reason: z.string().nullable().optional(),
  created_at: z.string(),
  updated_at: z.string(),
});

export const appSchema = z.looseObject({
  id: z.string(),
  title: z.string(),
  created_at: z.string(),
  admin_token: z.string().optional(),
  status: appStatusSchema.optional(),
  effective_status: appStatusSchema.optional(),
  /** Effective role: the higher of the app role and the org role (D0.7). */
  role: effectiveRoleSchema.optional(),
  org_id: z.string().nullable().optional(),
  pro: z.boolean().optional(),
  rules: z.unknown().optional(),
  rules_version: z.number().nullable().optional(),
  user_app_role: memberRoleSchema.nullable().optional(),
  members: z.array(appMemberSchema).nullable().optional(),
  invites: z.array(appInviteSchema).nullable().optional(),
  magic_code_email_template: emailTemplateSchema.nullable().optional(),
  magic_code_expiry_minutes: z.number().nullable().optional(),
  org: z
    .looseObject({ id: z.string(), title: z.string() })
    .nullable()
    .optional(),
  webhooks: z.array(webhookSchema).optional(),
});

export const orgSchema = z.looseObject({
  id: z.string(),
  title: z.string(),
  created_at: z.string(),
  updated_at: z.string().optional(),
});

export const orgSummarySchema = orgSchema.extend({
  role: effectiveRoleSchema,
  paid: z.boolean(),
});

export const pendingInviteSchema = z.looseObject({
  type: z.enum(['app', 'org']),
  id: z.string(),
  foreign_key: z.string(),
  title: z.string(),
  invitee_role: memberRoleSchema,
  inviter_email: z.string(),
});

/** The `GET /dash` bootstrap payload (D3.3). */
export const dashSchema = z.looseObject({
  apps: z.array(appSchema),
  superuser: z.boolean(),
  user: userSchema,
  orgs: z.array(orgSummarySchema).optional(),
  invites: z.array(pendingInviteSchema).optional(),
  profile: profileSchema.nullable().optional(),
  instant_config_app_id: z.string().optional(),
});

/** `GET /dash/orgs/:id` returns the org plus everything its pages show. */
export const orgDetailsSchema = z.looseObject({
  org: orgSummarySchema,
  apps: z.array(appSchema),
  members: z.array(appMemberSchema),
  invites: z.array(appInviteSchema),
});

export const appAuthResultSchema = z.looseObject({
  token: z.string(),
  user: userSchema,
  redirect_path: z.string().optional(),
});

export type App = z.infer<typeof appSchema>;
export type Dash = z.infer<typeof dashSchema>;
export type EffectiveRole = z.infer<typeof effectiveRoleSchema>;
export type OrgDetails = z.infer<typeof orgDetailsSchema>;
export type OrgSummary = z.infer<typeof orgSummarySchema>;
export type PendingInvite = z.infer<typeof pendingInviteSchema>;
export type Webhook = z.output<typeof webhookSchema>;
export type WebhookAction = z.output<typeof webhookActionSchema>;
