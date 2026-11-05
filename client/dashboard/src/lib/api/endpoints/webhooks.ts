import { z } from 'zod';
import {
  emptyResponseSchema,
  webhookActionSchema,
  webhookSchema,
} from './schemas';
import { defineEndpoint } from './types';

/**
 * The JVM server emitted hyphenated keys; this server returns `to_jsonb` of
 * the `webhook_attempt` composite, which keeps the column names. Accept both
 * so attempt details survive either wire shape.
 */
const webhookAttemptSchema = z.looseObject({
  'attempt-at': z.string().nullable().optional(),
  attempt_at: z.string().nullable().optional(),
  'duration-ms': z.number().nullable().optional(),
  duration_ms: z.number().nullable().optional(),
  'success?': z.boolean().nullable().optional(),
  success: z.boolean().nullable().optional(),
  'status-code': z.number().nullable().optional(),
  status_code: z.number().nullable().optional(),
  'response-text': z.string().nullable().optional(),
  response_text: z.string().nullable().optional(),
  'error-type': z.string().nullable().optional(),
  error_type: z.string().nullable().optional(),
  'error-message': z.string().nullable().optional(),
  error_message: z.string().nullable().optional(),
});

const webhookEventSchema = z.looseObject({
  isn: z.string(),
  status: z.string(),
  attempts: z.array(webhookAttemptSchema).nullable().optional(),
  next_attempt_after: z.string().nullable().optional(),
  created_at: z.string(),
  updated_at: z.string(),
});

const webhookPayloadRecordSchema = z.looseObject({
  namespace: z.string(),
  id: z.string(),
  action: webhookActionSchema,
  before: z.record(z.string(), z.unknown()).nullable().optional(),
  after: z.record(z.string(), z.unknown()).nullable().optional(),
  idempotencyKey: z.string(),
});

/** Webhook list, detail and deliveries (C.13). */
export const webhookEndpoints = {
  list: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/webhooks',
    response: z.looseObject({ webhooks: z.array(webhookSchema) }),
  }),

  create: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/webhooks',
    body: z.looseObject({
      url: z.string(),
      namespaces: z.array(z.string()),
      actions: z.array(webhookActionSchema),
    }),
    response: z.looseObject({ webhook: webhookSchema }),
  }),

  update: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/webhooks/:webhookId',
    body: z.looseObject({
      url: z.string().optional(),
      namespaces: z.array(z.string()).optional(),
      actions: z.array(webhookActionSchema).optional(),
    }),
    response: z.looseObject({ webhook: webhookSchema }),
  }),

  remove: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/webhooks/:webhookId',
    response: z.looseObject({ webhook: webhookSchema.optional() }),
  }),

  enable: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/webhooks/:webhookId/enable',
    body: emptyResponseSchema,
    response: z.looseObject({ webhook: webhookSchema }),
  }),

  disable: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/webhooks/:webhookId/disable',
    body: z.looseObject({ reason: z.string().optional() }),
    response: z.looseObject({ webhook: webhookSchema }),
  }),

  events: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/webhooks/:webhookId/events',
    response: z.looseObject({
      events: z.array(webhookEventSchema),
      pageInfo: z.looseObject({
        startCursor: z.string().nullable(),
        endCursor: z.string().nullable(),
        hasNextPage: z.boolean(),
      }),
    }),
  }),

  event: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/webhooks/:webhookId/events/:isn',
    response: z.looseObject({ event: webhookEventSchema }),
  }),

  resendEvent: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/webhooks/:webhookId/events/:isn',
    body: emptyResponseSchema,
    response: z.looseObject({ event: webhookEventSchema }),
  }),

  /** Payload snapshots live on the runtime API, not under `/dash`. */
  payload: defineEndpoint({
    method: 'GET',
    path: '/webhooks/payload/:appId/:webhookId/:isn',
    response: z.looseObject({
      data: z.array(webhookPayloadRecordSchema).nullable(),
      idempotencyKey: z.string(),
    }),
  }),
};

export type WebhookAttempt = z.output<typeof webhookAttemptSchema>;
export type WebhookEvent = z.output<typeof webhookEventSchema>;
export type WebhookEventsPage = z.output<
  typeof webhookEndpoints.events.response
>;
export type WebhookPayload = z.output<typeof webhookEndpoints.payload.response>;
export type WebhookPayloadRecord = z.output<typeof webhookPayloadRecordSchema>;
