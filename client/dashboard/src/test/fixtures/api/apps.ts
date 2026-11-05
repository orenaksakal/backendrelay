import type {
  BackupJob,
  BackupSchedule,
  RestoreJob,
} from '@/lib/api/endpoints';
import type { AppSummary } from '@/lib/api/endpoints/apps';
import type { App } from '@/lib/api/endpoints/schemas';
import { mockOrgApp } from './organization';

/**
 * C.4 fixtures. The personal workspace has one app per status and one app with
 * no write, no backup and no files, so every table branch has a row.
 */

export const mockTodoApp: App = {
  id: 'app-1',
  title: 'todo-app',
  created_at: '2026-01-01T00:00:00Z',
  status: 'active',
  role: 'owner',
  admin_token: 'app-1-admin-token-0123456789',
  members: [
    { id: 'app-member-1', email: 'owner@example.com', role: 'owner' },
    { id: 'app-member-2', email: 'admin@example.com', role: 'admin' },
    { id: 'app-member-3', email: 'dev@example.com', role: 'collaborator' },
  ],
  invites: [
    {
      id: 'app-invite-1',
      email: 'new@example.com',
      status: 'pending',
      role: 'collaborator',
      sent_at: '2026-02-01T00:00:00Z',
      expired: false,
    },
  ],
  rules: {
    todos: {
      allow: {
        view: 'true',
        create: 'true',
        update: 'auth.id != null',
        delete: 'false',
      },
    },
    goals: {
      allow: { create: 'false', update: 'false', delete: 'false' },
    },
  },
  rules_version: 3,
  magic_code_expiry_minutes: 15,
  webhooks: [
    {
      id: 'webhook-1',
      sink: { url: 'https://example.com/hooks/todo' },
      namespaces: ['todos'],
      actions: ['create'],
      status: 'disabled',
      disabled_reason: 'Repeated delivery failures',
      created_at: '2026-01-02T00:00:00Z',
      updated_at: '2026-01-03T00:00:00Z',
    },
    {
      id: 'webhook-2',
      sink: { url: 'https://example.com/hooks/goals' },
      namespaces: ['goals'],
      actions: ['update'],
      status: 'active',
      created_at: '2026-01-04T00:00:00Z',
      updated_at: '2026-01-05T00:00:00Z',
    },
  ],
  magic_code_email_template: {
    id: 'template-1',
    name: 'magic-code',
    email: 'noreply@example.com',
    subject: 'Your code',
    body: 'Use {code} to sign in.',
  },
};

export const mockReadOnlyApp: App = {
  id: 'app-2',
  title: 'headshot-ai',
  created_at: '2026-02-01T00:00:00Z',
  status: 'read-only',
  role: 'collaborator',
};

export const mockDisabledApp: App = {
  id: 'app-3',
  title: 'analytics',
  created_at: '2026-03-01T00:00:00Z',
  status: 'disabled',
  role: 'admin',
};

export const mockPersonalApps: App[] = [
  mockTodoApp,
  mockReadOnlyApp,
  mockDisabledApp,
];

export const mockAppSummaries: AppSummary[] = [
  {
    id: mockTodoApp.id,
    liveSessions: 12,
    appBytes: 4_404_019,
    storageBytes: 125_829_120,
    lastWriteAt: '2026-10-26T18:04:11Z',
    lastBackupAt: '2026-10-26T06:00:00Z',
  },
  {
    id: mockReadOnlyApp.id,
    liveSessions: 0,
    appBytes: 18_874_368,
    storageBytes: 2_254_857_830,
    lastWriteAt: '2026-10-24T09:30:00Z',
    lastBackupAt: null,
  },
  {
    id: mockDisabledApp.id,
    liveSessions: 3,
    appBytes: 0,
    storageBytes: 0,
    lastWriteAt: null,
    lastBackupAt: '2026-09-01T00:00:00Z',
  },
  {
    id: mockOrgApp.id,
    liveSessions: 2,
    appBytes: 1_048_576,
    storageBytes: 0,
    lastWriteAt: '2026-10-25T12:00:00Z',
    lastBackupAt: null,
  },
];

/** C.7 Schema fixtures: a running constraint check and the Deleted tab. */
export const mockIndexingJob = {
  id: 'job-1',
  app_id: 'app-1',
  group_id: null,
  attr_id: 'attr-title',
  attr_name: 'todos.title',
  job_type: 'unique',
  job_status: 'processing',
  job_stage: 'validate',
  work_estimate: 12_400,
  work_completed: 5_580,
  error: null,
  checked_data_type: null,
  created_at: '2026-10-27T10:00:00Z',
  updated_at: '2026-10-27T10:00:05Z',
  done_at: null,
  invalid_unique_value: null,
  invalid_triples_sample: [],
  error_data: null,
};

export const mockSoftDeletedAttributes = {
  attrs: [
    {
      id: 'attr-note',
      'forward-identity': ['ident-note', 'todos', 'note'],
      'value-type': 'blob',
      cardinality: 'one',
      'index?': false,
      'unique?': false,
      'required?': false,
      'checked-data-type': 'string',
      'deletion-marked-at': '2026-10-26T09:00:00Z',
    },
  ],
  'grace-period-days': 2,
};

/** C.10 History fixtures: each edit transforms version N into version N-1. */
export const mockRuleVersions = {
  versions: [
    {
      version: 3,
      created_at: '2026-10-26T18:00:00Z',
      edits: [[['todos', 'allow', 'view'], 'r', 'auth.id != null']],
    },
    {
      version: 2,
      created_at: '2026-10-25T12:00:00Z',
      edits: [[['todos', 'allow', 'create'], '-']],
    },
  ],
};

/** C.10 Test-as fixtures: one allowed and one denied debug check. */
export const mockQueryPermissionsCheck = {
  result: {
    todos: [{ id: 'todo-1', title: 'Buy milk', done: false }],
  },
  'check-results': [
    {
      entity: 'todos',
      id: 'todo-1',
      label: 'view',
      record: { id: 'todo-1', title: 'Buy milk', done: false },
      program: {
        action: 'view',
        code: 'x',
        'display-code': 'auth.id != null',
      },
      check: true,
    },
  ],
};

export const mockTransactPermissionsCheck = {
  'tx-id': 42,
  'all-checks-ok?': false,
  'committed?': false,
  'check-results': [
    {
      scope: 'object',
      action: 'update',
      etype: 'todos',
      eid: 'todo-1',
      program: {
        action: 'update',
        code: 'x',
        'display-code': 'auth.id != null',
      },
      'check-result': false,
      'check-pass?': false,
    },
  ],
};

export const mockAdminUsers = {
  data: {
    $users: [
      { id: 'user-1', email: 'owner@example.com' },
      { id: 'user-2', email: 'ada@example.com' },
    ],
  },
};

/** C.10 Editor fixtures: the collaborator-readable schema behind completions. */
export const mockSchemaPull = {
  'app-title': 'todo-app',
  attrs: [
    {
      id: 'attr-todos-id',
      'forward-identity': ['ident-todos-id', 'todos', 'id'],
      'value-type': 'blob',
      cardinality: 'one',
      'index?': true,
      'unique?': true,
      'required?': true,
      'checked-data-type': 'string',
      catalog: 'user',
    },
    {
      id: 'attr-todos-title',
      'forward-identity': ['ident-todos-title', 'todos', 'title'],
      'value-type': 'blob',
      cardinality: 'one',
      'index?': true,
      'unique?': false,
      'required?': false,
      'checked-data-type': 'string',
      catalog: 'user',
    },
    {
      id: 'attr-todos-done',
      'forward-identity': ['ident-todos-done', 'todos', 'done'],
      'value-type': 'blob',
      cardinality: 'one',
      'index?': false,
      'unique?': false,
      'required?': false,
      'checked-data-type': 'boolean',
      catalog: 'user',
    },
    {
      id: 'attr-todos-owner',
      'forward-identity': ['ident-todos-owner', 'todos', 'owner'],
      'reverse-identity': ['ident-users-todos', '$users', 'todos'],
      'value-type': 'ref',
      cardinality: 'one',
      'index?': false,
      'unique?': false,
      'required?': false,
      catalog: 'user',
    },
    {
      id: 'attr-users-id',
      'forward-identity': ['ident-users-id', '$users', 'id'],
      'value-type': 'blob',
      cardinality: 'one',
      'index?': true,
      'unique?': true,
      'required?': true,
      'checked-data-type': 'string',
      catalog: 'system',
    },
    {
      id: 'attr-users-email',
      'forward-identity': ['ident-users-email', '$users', 'email'],
      'value-type': 'blob',
      cardinality: 'one',
      'index?': true,
      'unique?': true,
      'required?': false,
      'checked-data-type': 'string',
      catalog: 'system',
    },
  ],
};

/** C.5 overview fixtures: the live snapshot, usage, backups and auth status. */
export const mockAppStats = {
  count: 12,
  origins: {
    'https://app.example.com': 8,
    'https://admin.example.com': 4,
  },
};

export const mockAppUsage = {
  appBytes: 4_404_019,
  storageBytes: 125_829_120,
  fileCount: 240,
  liveSessions: 12,
  transactions: {
    last24h: 87,
    last7d: 512,
    daily: [
      { date: '2026-10-20', count: 61 },
      { date: '2026-10-21', count: 74 },
      { date: '2026-10-22', count: 70 },
      { date: '2026-10-23', count: 66 },
      { date: '2026-10-24', count: 82 },
      { date: '2026-10-25', count: 72 },
      { date: '2026-10-26', count: 87 },
    ],
  },
};

export const mockBackups = [
  {
    id: 'backup-1',
    isn: null,
    backup_at: '2026-10-26T18:00:00Z',
    files_size: 125_829_120,
    db_size: 4_404_019,
    uncompressed_size: 131_000_000,
    description: 'Nightly',
    expires_at: '2026-11-26T18:00:00Z',
  },
  {
    id: 'backup-2',
    isn: null,
    backup_at: '2026-10-19T18:00:00Z',
    files_size: 118_000_000,
    db_size: 4_100_000,
    uncompressed_size: 122_500_000,
    description: 'Before release',
    expires_at: null,
  },
];

export const mockBackupFiles = [
  { name: 'config.json', size: 2_048 },
  { name: 'entities/todos.jsonl', size: 4_194_304 },
  { name: 'entities/goals.jsonl', size: 131_072 },
];

export const mockBackupStorageFiles = [
  {
    locationId: 'loc-1',
    path: 'avatars/me.png',
    url: 'https://files.example.com/avatars/me.png',
  },
];

export const mockBackupJobs: BackupJob[] = [];

export const mockActiveBackupJobs: BackupJob[] = [
  {
    id: 'backup-job-1',
    app_id: 'app-1',
    app_backup_id: null,
    job_status: 'processing',
    description: 'Before migration',
    work_estimate: 100,
    work_completed: 40,
    created_at: '2026-10-27T08:00:00Z',
    updated_at: '2026-10-27T08:01:00Z',
    done_at: null,
  },
];

export const mockEnabledBackupSchedule: BackupSchedule = {
  cron: '0 3 * * *',
  enabled: true,
  next_run_at: '2026-10-28T03:00:00Z',
  last_enqueued_at: '2026-10-27T03:00:00Z',
};

export const mockAppRestoreJobs: RestoreJob[] = [
  {
    id: 'restore-job-1',
    app_id: 'app-restored',
    title: 'todo-app (restored 2026-10-20)',
    job_status: 'done',
    progress: '4,800 / 4,800 entities',
    error: null,
    created_at: '2026-10-20T09:00:00Z',
    done_at: '2026-10-20T09:04:00Z',
    updated_at: '2026-10-20T09:04:00Z',
  },
];

export const mockBackupSchedule = { schedule: null };

export const mockSenderVerification = {
  instant: { 'verified?': false },
  verification: null,
  domain: null,
};
