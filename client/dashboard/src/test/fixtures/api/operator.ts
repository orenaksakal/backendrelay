import type {
  KnownOperatorFlag,
  OperatorFlags,
  RestoreJob,
} from '@/lib/api/endpoints';

/**
 * C.19 fixtures: the typed groups, a few documented flags for the advanced
 * table, one undocumented flag and restore jobs in every status.
 */
export const mockKnownOperatorFlags: KnownOperatorFlag[] = [
  {
    name: 'dashboard-signups',
    type: 'object',
    description:
      'Signup policy: {"mode": "open"|"restricted"|"closed", "allowedEmails": [...]}.',
  },
  {
    name: 'ephemeral-apps-enabled',
    type: 'boolean',
    description: 'Enables unauthenticated ephemeral app creation.',
  },
  {
    name: 'magic-code-rate-limit-per-hour',
    type: 'integer',
    description: 'Hourly magic-code send limit per app and email.',
  },
  {
    name: 'default-magic-code-expiry-minutes',
    type: 'integer',
    description: 'Default magic-code expiry in minutes.',
  },
  {
    name: 'smokescreen-whitelist-ips',
    type: 'array',
    description: 'IP addresses or CIDRs allowed past the webhook SSRF guard.',
  },
  {
    name: 'refresh-throttled-apps',
    type: 'array',
    description: 'App UUIDs subject to the refresh throttle.',
  },
  {
    name: 'refresh-throttle-ms',
    type: 'number',
    description: 'Refresh throttle window in milliseconds.',
  },
  {
    name: 'row-hint-threshold',
    type: 'number',
    description:
      'Row-count threshold above which PostgreSQL planner hints are applied.',
  },
  {
    name: 'team-emails',
    type: 'array',
    description:
      'Array of {"email": "..."} objects that grant dashboard operator rights.',
  },
  {
    name: 'storage-quota-bytes',
    type: 'integer',
    description:
      'Deployment-wide soft storage quota in bytes; unset means unlimited.',
  },
];

export const mockOperatorFlags: OperatorFlags = {
  flags: {
    'dashboard-signups': {
      mode: 'restricted',
      allowedEmails: ['dev@example.com', 'ops@example.com'],
    },
    'ephemeral-apps-enabled': true,
    'magic-code-rate-limit-per-hour': 25,
    'default-magic-code-expiry-minutes': 15,
    'smokescreen-whitelist-ips': ['10.0.0.25'],
    // The fixture apps use short IDs; the checkbox list reads from bootstrap.
    'refresh-throttled-apps': ['app-1'],
    'refresh-throttle-ms': 750,
    'row-hint-threshold': 250000,
    'team-emails': [{ email: 'ops@example.com' }],
    'storage-quota-bytes': 1_073_741_824,
    // Stored but absent from the server's known list.
    'legacy-flag': true,
  },
  known: mockKnownOperatorFlags,
};

export const mockRestoreJobs: RestoreJob[] = [
  {
    id: '3f2a9c1d-4b7e-4c2a-9d31-8e5f6a7b8c9d',
    app_id: 'app-2',
    title: 'headshot-ai copy',
    job_status: 'processing',
    progress: '1,200 / 4,800 entities',
    error: null,
    created_at: '2026-10-26T20:10:00Z',
    done_at: null,
    updated_at: '2026-10-26T20:11:00Z',
  },
  {
    id: '7b1d2c3e-5a6f-4b8c-9d0e-1f2a3b4c5d6e',
    app_id: 'app-1',
    title: 'todo-app restore',
    job_status: 'waiting',
    progress: null,
    error: null,
    created_at: '2026-10-26T20:05:00Z',
    done_at: null,
    updated_at: '2026-10-26T20:05:00Z',
  },
  {
    id: '8c3e4d5f-6b7a-4c9d-8e1f-2a3b4c5d6e7f',
    app_id: 'app-3',
    title: 'analytics restore',
    job_status: 'done',
    progress: '4,800 / 4,800 entities',
    error: null,
    created_at: '2026-10-25T18:00:00Z',
    done_at: '2026-10-25T18:04:00Z',
    updated_at: '2026-10-25T18:04:00Z',
  },
  {
    id: 'c9d4e5f6-7a8b-4c0d-9e2f-3a4b5c6d7e8f',
    app_id: 'app-2',
    title: null,
    job_status: 'failed',
    progress: null,
    error: 'The backup ZIP is missing.',
    created_at: '2026-10-24T09:00:00Z',
    done_at: '2026-10-24T09:01:00Z',
    updated_at: '2026-10-24T09:01:00Z',
  },
  {
    id: 'd0e5f6a7-8b9c-4d1e-8f3a-4b5c6d7e8f9a',
    app_id: 'app-1',
    title: 'headshot-ai restore (cancelled)',
    job_status: 'cancelled',
    progress: '300 / 4,800 entities',
    error: null,
    created_at: '2026-10-23T12:00:00Z',
    done_at: '2026-10-23T12:02:00Z',
    updated_at: '2026-10-23T12:02:00Z',
  },
];
