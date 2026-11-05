import type { Webhook, WebhookEvent } from '@/lib/api/endpoints';

/**
 * C.13 fixtures: one active webhook with four namespaces (so the "+1" badge
 * renders), one disabled webhook with a reason, and deliveries in every state
 * including one outside the 30-day resend window. Times are relative to load
 * so the resend window test never ages out.
 */

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const NOW = Date.now();

function at(offsetMs: number): string {
  return new Date(NOW + offsetMs).toISOString();
}

export const mockWebhooks: Webhook[] = [
  {
    id: 'webhook-1',
    sink: { url: 'https://example.com/hooks/instant' },
    namespaces: ['todos', '$files', '$users', 'notes'],
    actions: ['create', 'update'],
    status: 'active',
    disabled_reason: null,
    created_at: '2026-10-20T10:00:00Z',
    updated_at: '2026-10-26T12:00:00Z',
  },
  {
    id: 'webhook-2',
    sink: { url: 'https://hooks.example.net/receive' },
    namespaces: ['todos'],
    actions: ['delete'],
    status: 'disabled',
    disabled_reason: 'Downstream maintenance',
    created_at: '2026-10-10T10:00:00Z',
    updated_at: '2026-10-25T09:00:00Z',
  },
];

export const mockWebhookEvents: WebhookEvent[] = [
  {
    isn: '0/328/48953748',
    status: 'success',
    attempts: [
      {
        'attempt-at': at(-MINUTE + 1000),
        'duration-ms': 120,
        'success?': true,
        'status-code': 200,
        'response-text': 'ok',
      },
    ],
    next_attempt_after: null,
    created_at: at(-MINUTE),
    updated_at: at(-MINUTE + 1000),
  },
  {
    isn: '0/329/48953750',
    status: 'failed',
    attempts: [
      {
        'attempt-at': at(-2 * HOUR + 20 * MINUTE),
        'duration-ms': 40,
        'success?': false,
        'status-code': 503,
        'response-text': 'Service Unavailable',
      },
      {
        'attempt-at': at(-2 * HOUR + 10 * MINUTE),
        'duration-ms': 30,
        'success?': false,
        'status-code': 503,
        'response-text': 'Service Unavailable',
      },
    ],
    next_attempt_after: null,
    created_at: at(-2 * HOUR),
    updated_at: at(-2 * HOUR + 20 * MINUTE),
  },
  {
    isn: '0/330/48953751',
    status: 'pending',
    attempts: [],
    next_attempt_after: at(-3 * HOUR + MINUTE),
    created_at: at(-3 * HOUR),
    updated_at: at(-3 * HOUR),
  },
  {
    isn: '0/301/11111111',
    status: 'success',
    attempts: [
      {
        'attempt-at': at(-31 * DAY + 1000),
        'duration-ms': 95,
        'success?': true,
        'status-code': 204,
        'response-text': '',
      },
    ],
    next_attempt_after: null,
    created_at: at(-31 * DAY),
    updated_at: at(-31 * DAY + 1000),
  },
];

export const mockWebhookOlderEvents: WebhookEvent[] = [
  {
    isn: '0/327/48953740',
    status: 'success',
    attempts: [
      {
        'attempt-at': at(-40 * DAY + 1000),
        'duration-ms': 88,
        'success?': true,
        'status-code': 200,
        'response-text': 'ok',
      },
    ],
    next_attempt_after: null,
    created_at: at(-40 * DAY),
    updated_at: at(-40 * DAY + 1000),
  },
];

export const mockWebhookPayload = {
  idempotencyKey: 'idem-48953748',
  data: [
    {
      namespace: 'todos',
      id: 'todo-1',
      action: 'create',
      before: null,
      after: { id: 'todo-1', title: 'Buy milk' },
      idempotencyKey: 'idem-48953748',
    },
  ],
};

/** The payload reader returns `data: null` when the snapshot aged out. */
export const mockMissingWebhookPayload = {
  idempotencyKey: 'idem-11111111',
  data: null,
};
