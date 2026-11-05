import { describe, expect, it } from 'vitest';
import type { Webhook, WebhookEvent } from '@/lib/api/endpoints';
import { mockWebhooks } from '@/test/fixtures/api/webhooks';
import {
  MAX_ACTIVE_WEBHOOKS,
  activeWebhookCount,
  activeWebhookLimitReached,
  anyEventInFlight,
  attemptStatusCode,
  attemptsNewestFirst,
  canResendEvent,
  deliveryAttemptCount,
  deliveryDisplay,
  deliveryDuration,
  deliveryHttpCode,
  isEventInFlight,
  isPublicHost,
  jwksUrl,
  latestAttempt,
  latestEvent,
  namespaceOptions,
  namespacesForDisplay,
  normalizeActions,
  sortedNamespaces,
  sortedWebhooks,
  verificationSnippet,
  webhookBody,
  webhookDraft,
  webhookDraftProblem,
  webhookLimitNote,
  webhookUrlProblem,
} from './model';

const NOW = new Date('2026-10-27T12:00:00Z');

function event(overrides: Partial<WebhookEvent>): WebhookEvent {
  return {
    isn: '0/1/1',
    status: 'success',
    attempts: [],
    next_attempt_after: null,
    created_at: '2026-10-27T11:00:00Z',
    updated_at: '2026-10-27T11:00:00Z',
    ...overrides,
  };
}

function webhook(overrides: Partial<Webhook>): Webhook {
  return {
    id: 'webhook-1',
    sink: { url: 'https://example.com/hooks' },
    namespaces: ['todos'],
    actions: ['create'],
    status: 'active',
    disabled_reason: null,
    created_at: '2026-10-20T10:00:00Z',
    updated_at: '2026-10-20T10:00:00Z',
    ...overrides,
  };
}

describe('webhook URL validation', () => {
  it('accepts a public https URL', () => {
    expect(
      webhookUrlProblem('https://example.com/webhooks/instant?x=1'),
    ).toBeUndefined();
  });

  it('requires https', () => {
    expect(webhookUrlProblem('http://example.com/hooks')).toBe(
      'The webhook URL must use https.',
    );
  });

  it('rejects blank and malformed values', () => {
    expect(webhookUrlProblem('   ')).toBe('Enter a webhook URL.');
    expect(webhookUrlProblem('not a url')).toBe('Enter a valid URL.');
  });

  it.each([
    'https://localhost/hooks',
    'https://api.localhost/hooks',
    'https://printer.local/hooks',
    'https://127.0.0.1/hooks',
    'https://0.0.0.0/hooks',
    'https://10.2.3.4/hooks',
    'https://172.16.9.1/hooks',
    'https://192.168.1.10/hooks',
    'https://169.254.169.254/latest/meta-data',
    'https://100.64.0.9/hooks',
    'https://198.18.0.1/hooks',
    'https://[::1]/hooks',
    'https://[fd00::1]/hooks',
    'https://[fe80::1]/hooks',
    'https://[::ffff:10.0.0.1]/hooks',
    'https://[::ffff:7f00:1]/hooks',
    'https://intranet/hooks',
  ])('rejects private destinations (%s)', (url) => {
    expect(webhookUrlProblem(url)).toBe(
      'The webhook URL must point at a public host.',
    );
  });

  it('classifies hosts without parsing the path', () => {
    expect(isPublicHost('example.com.')).toBe(true);
    expect(isPublicHost('EXAMPLE.COM')).toBe(true);
    expect(isPublicHost('100.63.255.255')).toBe(true);
    expect(isPublicHost('100.127.255.255')).toBe(false);
  });
});

describe('webhook drafts', () => {
  it('reads the edit form from the webhook, sorted and canonical', () => {
    expect(
      webhookDraft(
        webhook({
          sink: { url: 'https://example.com/hooks' },
          namespaces: ['zebra', 'todos', 'todos'],
          actions: ['delete', 'create'],
        }),
      ),
    ).toEqual({
      url: 'https://example.com/hooks',
      namespaces: ['todos', 'zebra'],
      actions: ['create', 'delete'],
    });
  });

  it('defaults a new webhook to create-only', () => {
    expect(webhookDraft()).toEqual({
      url: '',
      namespaces: [],
      actions: ['create'],
    });
  });

  it('requires a URL, a namespace and an action', () => {
    expect(
      webhookDraftProblem({
        url: 'https://127.0.0.1/hooks',
        namespaces: ['todos'],
        actions: ['create'],
      }),
    ).toBe('The webhook URL must point at a public host.');
    expect(
      webhookDraftProblem({
        url: 'https://example.com/hooks',
        namespaces: [],
        actions: ['create'],
      }),
    ).toBe('Select at least one namespace.');
    expect(
      webhookDraftProblem({
        url: 'https://example.com/hooks',
        namespaces: ['todos'],
        actions: [],
      }),
    ).toBe('Select at least one action.');
    expect(
      webhookDraftProblem({
        url: ' https://example.com/hooks ',
        namespaces: ['todos'],
        actions: ['create'],
      }),
    ).toBeUndefined();
  });

  it('trims and canonicalises the body', () => {
    expect(
      webhookBody({
        url: '  https://example.com/hooks ',
        namespaces: ['zebra', 'todos'],
        actions: ['update', 'create'],
      }),
    ).toEqual({
      url: 'https://example.com/hooks',
      namespaces: ['todos', 'zebra'],
      actions: ['create', 'update'],
    });
  });

  it('keeps a referencing namespace that left the schema', () => {
    expect(namespaceOptions(['todos', '$users'], ['gone'])).toEqual([
      '$users',
      'gone',
      'todos',
    ]);
    expect(sortedNamespaces(['b', 'a', 'b'])).toEqual(['a', 'b']);
    expect(normalizeActions(['delete', 'create'])).toEqual([
      'create',
      'delete',
    ]);
  });
});

describe('webhook list helpers', () => {
  it('counts active webhooks against the server cap', () => {
    const active = Array.from({ length: MAX_ACTIVE_WEBHOOKS }, (_, index) =>
      webhook({ id: `webhook-${String(index)}` }),
    );
    expect(activeWebhookCount(mockWebhooks)).toBe(1);
    expect(activeWebhookLimitReached(mockWebhooks)).toBe(false);
    expect(activeWebhookLimitReached(active)).toBe(true);
    expect(webhookLimitNote([...active, webhook({ status: 'disabled' })])).toBe(
      '100 of 100 active webhooks used.',
    );
  });

  it('sorts newest first and keeps a stable tie-break', () => {
    expect(sortedWebhooks(mockWebhooks).map((entry) => entry.id)).toEqual([
      'webhook-1',
      'webhook-2',
    ]);
  });

  it('shows at most three namespace badges', () => {
    expect(namespacesForDisplay(['a', 'b', 'c', 'd'])).toEqual({
      shown: ['a', 'b', 'c'],
      extra: 1,
    });
    expect(namespacesForDisplay(null)).toEqual({ shown: [], extra: 0 });
  });
});

describe('delivery helpers', () => {
  it('maps every event status to a label and tone', () => {
    expect(deliveryDisplay('success')).toEqual({
      label: 'Delivered',
      tone: 'success',
    });
    expect(deliveryDisplay('error').label).toBe('Failed');
    expect(deliveryDisplay('failed').label).toBe('Failed');
    expect(deliveryDisplay('pending')).toEqual({
      label: 'Pending',
      tone: 'warning',
    });
    expect(deliveryDisplay('processing').label).toBe('Pending');
    expect(deliveryDisplay('unknown-status').label).toBe('Pending');
  });

  it('reports in-flight states', () => {
    expect(isEventInFlight(event({ status: 'pending' }))).toBe(true);
    expect(isEventInFlight(event({ status: 'processing' }))).toBe(true);
    expect(isEventInFlight(event({ status: 'success' }))).toBe(false);
    expect(
      anyEventInFlight([
        event({ status: 'success' }),
        event({ status: 'processing' }),
      ]),
    ).toBe(true);
    expect(anyEventInFlight([])).toBe(false);
  });

  it('reads attempts newest first with their HTTP code and duration', () => {
    const delivery = event({
      attempts: [
        {
          'attempt-at': '2026-10-27T11:00:00Z',
          'duration-ms': 30,
          'success?': false,
          'status-code': 503,
        },
        {
          'attempt-at': '2026-10-27T11:05:00Z',
          'duration-ms': 12,
          'success?': false,
          'status-code': 500,
        },
      ],
    });
    expect(
      attemptsNewestFirst(delivery).map((attempt) => attempt['status-code']),
    ).toEqual([500, 503]);
    expect(latestAttempt(delivery)?.['status-code']).toBe(500);
    expect(deliveryAttemptCount(delivery)).toBe(2);
    expect(deliveryHttpCode(delivery)).toBe(500);
    expect(deliveryDuration(delivery)).toBe(12);
    expect(deliveryHttpCode(event({}))).toBeUndefined();
  });

  it('reads the snake_case attempt shape this server returns', () => {
    const delivery = event({
      attempts: [
        {
          attempt_at: '2026-10-27T11:05:00Z',
          duration_ms: 44,
          success: false,
          status_code: 502,
          response_text: 'Bad Gateway',
          error_type: 'http-status',
        },
      ],
    });
    expect(attemptStatusCode(attemptsNewestFirst(delivery)[0] ?? {})).toBe(502);
    expect(deliveryHttpCode(delivery)).toBe(502);
    expect(deliveryDuration(delivery)).toBe(44);
  });

  it('picks the newest event for the last-delivery column', () => {
    expect(
      latestEvent([
        event({ isn: '0/2/2', created_at: '2026-10-27T10:00:00Z' }),
        event({ isn: '0/3/3', created_at: '2026-10-27T12:00:00Z' }),
      ])?.isn,
    ).toBe('0/3/3');
    expect(latestEvent([])).toBeUndefined();
  });

  it('allows resend only inside the 30-day window', () => {
    expect(canResendEvent(event({}), NOW)).toBe(true);
    expect(
      canResendEvent(event({ created_at: '2026-08-01T09:00:00Z' }), NOW),
    ).toBe(false);
    expect(canResendEvent(event({ created_at: 'not a date' }), NOW)).toBe(
      false,
    );
  });
});

describe('signing helpers', () => {
  it('builds the JWKS URL without doubling slashes', () => {
    expect(jwksUrl('https://api.example.com')).toBe(
      'https://api.example.com/.well-known/webhooks/jwks.json',
    );
    expect(jwksUrl('https://api.example.com/')).toBe(
      'https://api.example.com/.well-known/webhooks/jwks.json',
    );
  });

  it('writes a verification snippet with this app and API origin', () => {
    const snippet = verificationSnippet({
      appId: 'app-1',
      apiBaseUrl: 'https://api.example.com/',
    });
    expect(snippet).toContain(
      "import { Webhooks } from '@instantdb/webhooks';",
    );
    expect(snippet).toContain("appId: 'app-1'");
    expect(snippet).toContain("apiURI: 'https://api.example.com'");
    expect(snippet).toContain('validateRequest');
    expect(snippet).toContain('fetchPayloads');
  });
});
