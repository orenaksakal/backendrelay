import { describe, expect, it } from 'vitest';
import {
  appHasCustomSender,
  buildOverviewHealth,
  effectiveAllowRule,
  lastSevenDays,
  publicWriteNamespaces,
  ruleAllowsEveryone,
  sessionOriginRows,
  statsPollInterval,
  STATS_POLL_INTERVAL_MS,
} from './overview';

const NOW = new Date('2026-10-26T18:05:00Z');

describe('overview permission rules', () => {
  const rules = {
    todos: { allow: { create: 'false', update: '$default' } },
    goals: { allow: { create: 'false', update: 'false', delete: 'false' } },
    $default: { allow: { update: 'false' } },
    $files: {},
  };

  it('follows the server inheritance order', () => {
    expect(effectiveAllowRule(rules, 'todos', 'create')).toBe('false');
    expect(effectiveAllowRule(rules, 'todos', 'update')).toBe('false');
    expect(effectiveAllowRule(rules, 'todos', 'delete')).toBe(true);
    expect(effectiveAllowRule(rules, '$files', 'delete')).toBe(false);
    expect(
      effectiveAllowRule({ todos: { allow: 'false' } }, 'todos', 'create'),
    ).toBe('false');
  });

  it('treats only an explicit true as public', () => {
    expect(ruleAllowsEveryone(true)).toBe(true);
    expect(ruleAllowsEveryone('true')).toBe(true);
    expect(ruleAllowsEveryone(' auth.id != null ')).toBe(false);
    expect(ruleAllowsEveryone(false)).toBe(false);
  });

  it('lists namespaces whose writes allow everyone', () => {
    expect(
      publicWriteNamespaces({
        todos: { allow: { create: 'true' } },
        goals: { allow: { create: 'false', update: 'false', delete: 'false' } },
        $files: {},
      }),
    ).toEqual(['todos']);
  });

  it('excludes public user registration without hiding other public writes', () => {
    expect(
      publicWriteNamespaces({
        $users: { allow: { create: 'true' } },
        todos: { allow: { create: 'true' } },
        $files: { allow: { create: true } },
      }),
    ).toEqual(['todos', '$files']);
  });

  it.each(['update', 'delete'] as const)(
    'still lists $users when %s is public',
    (action) => {
      expect(
        publicWriteNamespaces({
          $users: { allow: { create: 'true', [action]: 'true' } },
        }),
      ).toEqual(['$users']);
    },
  );
});

describe('overview health', () => {
  it('warns about public writes, backups, webhooks and an unverified sender', () => {
    const checks = buildOverviewHealth({
      appId: 'app-1',
      rules: { todos: { allow: { create: 'true' } } },
      webhooks: [{ status: 'disabled' }, { status: 'active' }],
      canAdmin: true,
      hasCustomSender: true,
      senderVerified: false,
      senderLoaded: true,
      lastBackupAt: '2026-10-26T18:00:00Z',
      backupsLoaded: true,
      backupsFailed: false,
      hasBackupSchedule: false,
      scheduleLoaded: true,
      now: NOW,
    });

    expect(checks.map((check) => check.tone)).toEqual([
      'warning',
      'warning',
      'warning',
      'warning',
    ]);
    expect(checks[0]?.text).toBe(
      'Permissions: 1 namespace allows public writes',
    );
    expect(checks[1]?.text).toBe(
      'Backups: last backup 5 min ago · no schedule',
    );
    expect(checks[2]?.text).toBe('Webhooks: 1 disabled');
    expect(checks[3]?.text).toBe(
      'Auth: custom sender domain not verified (using the default BackendRelay sender)',
    );
  });

  it('stays quiet when permissions, backups and webhooks are healthy', () => {
    const checks = buildOverviewHealth({
      appId: 'app-1',
      rules: {
        todos: { allow: { create: 'false', update: 'false', delete: 'false' } },
      },
      webhooks: [{ status: 'active' }],
      canAdmin: true,
      hasCustomSender: false,
      senderVerified: undefined,
      senderLoaded: true,
      lastBackupAt: '2026-10-26T18:00:00Z',
      backupsLoaded: true,
      backupsFailed: false,
      hasBackupSchedule: true,
      scheduleLoaded: true,
      now: NOW,
    });

    expect(checks.map((check) => check.tone)).toEqual(['ok', 'ok', 'ok', 'ok']);
    expect(checks[1]?.text).toBe('Backups: last backup 5 min ago · scheduled');
    expect(checks[3]?.text).toBe('Auth: using the default BackendRelay sender');
  });

  it('does not warn when only user registration is public', () => {
    const checks = buildOverviewHealth({
      appId: 'app-1',
      rules: {
        $default: { allow: { $default: 'false' } },
        $users: { allow: { create: 'true' } },
      },
      webhooks: [],
      canAdmin: true,
      hasCustomSender: false,
      senderVerified: undefined,
      senderLoaded: true,
      lastBackupAt: '2026-10-26T18:00:00Z',
      backupsLoaded: true,
      backupsFailed: false,
      hasBackupSchedule: true,
      scheduleLoaded: true,
      now: NOW,
    });

    expect(checks[0]?.tone).toBe('ok');
    expect(checks[0]?.text).toBe('Permissions: no unexpected public writes');
    expect(checks[0]?.detail).toBeUndefined();
  });

  it('warns when no rules are configured, rather than claiming writes are safe', () => {
    const checks = buildOverviewHealth({
      appId: 'app-1',
      rules: {},
      webhooks: [],
      canAdmin: true,
      hasCustomSender: false,
      senderVerified: undefined,
      senderLoaded: true,
      lastBackupAt: '2026-10-26T18:00:00Z',
      backupsLoaded: true,
      backupsFailed: false,
      hasBackupSchedule: true,
      scheduleLoaded: true,
      now: NOW,
    });

    expect(checks[0]?.tone).toBe('warning');
    expect(checks[0]?.text).toBe(
      'Permissions: no rules configured; user namespaces allow public writes',
    );
  });

  it('warns when the app-level default allows public writes', () => {
    const checks = buildOverviewHealth({
      appId: 'app-1',
      rules: { $default: { allow: { create: 'true' } } },
      webhooks: [],
      canAdmin: true,
      hasCustomSender: false,
      senderVerified: undefined,
      senderLoaded: true,
      lastBackupAt: '2026-10-26T18:00:00Z',
      backupsLoaded: true,
      backupsFailed: false,
      hasBackupSchedule: true,
      scheduleLoaded: true,
      now: NOW,
    });

    expect(checks[0]?.tone).toBe('warning');
    expect(checks[0]?.text).toBe(
      'Permissions: the app default allows public writes',
    );
  });

  it('marks loading health rows as pending instead of warning', () => {
    const checks = buildOverviewHealth({
      appId: 'app-1',
      rules: {},
      webhooks: [],
      canAdmin: false,
      hasCustomSender: false,
      senderVerified: undefined,
      senderLoaded: false,
      lastBackupAt: null,
      backupsLoaded: false,
      backupsFailed: false,
      hasBackupSchedule: false,
      scheduleLoaded: false,
      now: NOW,
    });

    expect(checks[1]?.tone).toBe('pending');
    expect(checks[1]?.text).toBe('Backups: checking…');
    expect(checks[3]?.text).toBe(
      'Auth: sender verification needs admin access',
    );
  });
});

describe('overview metrics', () => {
  it('polls while visible and stops on a hidden tab', () => {
    expect(statsPollInterval(false)).toBe(STATS_POLL_INTERVAL_MS);
    expect(statsPollInterval(true)).toBe(false);
  });

  it('keeps the last seven days of the daily series', () => {
    const daily = Array.from({ length: 14 }, (_, index) => ({
      date: `2026-10-${String(index + 1).padStart(2, '0')}`,
      count: index,
    }));
    expect(lastSevenDays(daily)).toHaveLength(7);
    expect(lastSevenDays(daily)[0]?.count).toBe(7);
  });

  it('sorts session origins by traffic', () => {
    expect(
      sessionOriginRows({
        origins: {
          'https://small.example.com': 2,
          'https://large.example.com': 9,
        },
      }),
    ).toEqual([
      { origin: 'https://large.example.com', sessions: 9 },
      { origin: 'https://small.example.com', sessions: 2 },
    ]);
  });

  it('detects a custom email sender', () => {
    expect(
      appHasCustomSender({
        magic_code_email_template: {
          id: 'template-1',
          name: 'magic-code',
          subject: 'Your code',
          body: '{code}',
          email: 'noreply@example.com',
        },
      }),
    ).toBe(true);
    expect(appHasCustomSender({})).toBe(false);
  });
});
