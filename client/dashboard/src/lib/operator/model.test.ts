import { describe, expect, it } from 'vitest';
import { mockKnownOperatorFlags } from '@/test/fixtures/api/operator';
import {
  advancedFlagRows,
  draftFromFlags,
  formatFlagValue,
  hasDraftProblems,
  isIpOrCidr,
  isRestoreJobActive,
  jsonValuesEqual,
  normaliseAdvancedChange,
  operatorDraftProblems,
  parseLineList,
  restoreJobLabel,
  savePlanFromDraft,
} from './model';

describe('operator draft', () => {
  it('uses the server fallbacks when every flag is unset', () => {
    const draft = draftFromFlags({});

    expect(draft.signupMode).toBe('open');
    expect(draft.allowedEmailsText).toBe('');
    expect(draft.ephemeralApps).toBe(true);
    expect(draft.magicCodeRateLimit).toBe('20');
    expect(draft.magicCodeExpiry).toBe('15');
    expect(draft.webhookIpsText).toBe('');
    expect(draft.throttledApps).toEqual([]);
    expect(draft.throttleMs).toBe('1000');
    expect(draft.advanced).toEqual({});
  });

  it('normalises stored lists and the signup policy', () => {
    const draft = draftFromFlags({
      'dashboard-signups': {
        mode: 'restricted',
        allowedEmails: [
          ' Ops@Example.com ',
          'ops@example.com',
          'dev@example.com',
        ],
      },
      'smokescreen-whitelist-ips': ['10.0.0.25', ' 192.168.0.0/24 '],
      'refresh-throttled-apps': ['app-1', 'app-1'],
      'ephemeral-apps-enabled': false,
    });

    expect(draft.signupMode).toBe('restricted');
    expect(draft.allowedEmailsText).toBe('dev@example.com\nops@example.com');
    expect(draft.webhookIpsText).toBe('10.0.0.25\n192.168.0.0/24');
    expect(draft.throttledApps).toEqual(['app-1']);
    expect(draft.ephemeralApps).toBe(false);
  });

  it('saves nothing when nothing changed', () => {
    const flags = {
      'dashboard-signups': { mode: 'open', allowedEmails: [] },
      'refresh-throttle-ms': 750,
    };
    const plan = savePlanFromDraft(flags, draftFromFlags(flags));

    expect(plan).toEqual({ writes: [], resets: [] });
  });

  it('writes only changed values, in a stable order', () => {
    const flags = {
      'dashboard-signups': {
        mode: 'restricted',
        allowedEmails: ['dev@example.com', 'ops@example.com'],
      },
      'magic-code-rate-limit-per-hour': 25,
      'smokescreen-whitelist-ips': ['10.0.0.25'],
      'refresh-throttled-apps': ['app-1'],
      'refresh-throttle-ms': 750,
    };
    const draft = draftFromFlags(flags);
    draft.signupMode = 'closed';
    draft.magicCodeRateLimit = '30';
    draft.webhookIpsText = '10.0.0.25\n10.0.0.26';
    draft.throttledApps = ['app-2'];
    draft.throttleMs = '1200';

    const plan = savePlanFromDraft(flags, draft);

    expect(plan.resets).toEqual([]);
    expect(plan.writes).toEqual([
      {
        name: 'dashboard-signups',
        value: {
          mode: 'closed',
          allowedEmails: ['dev@example.com', 'ops@example.com'],
        },
      },
      { name: 'magic-code-rate-limit-per-hour', value: 30 },
      {
        name: 'smokescreen-whitelist-ips',
        value: ['10.0.0.25', '10.0.0.26'],
      },
      { name: 'refresh-throttled-apps', value: ['app-2'] },
      { name: 'refresh-throttle-ms', value: 1200 },
    ]);
  });

  it('stages advanced sets and resets into the same plan', () => {
    const flags = {
      'storage-quota-bytes': 1_073_741_824,
      'team-emails': [{ email: 'ops@example.com' }],
    };
    const draft = draftFromFlags(flags);
    draft.advanced['team-emails'] = {
      action: 'set',
      value: [{ email: 'new@example.com' }],
    };
    draft.advanced['storage-quota-bytes'] = { action: 'reset' };

    const plan = savePlanFromDraft(flags, draft);

    expect(plan.resets).toEqual(['storage-quota-bytes']);
    expect(plan.writes).toEqual([
      { name: 'team-emails', value: [{ email: 'new@example.com' }] },
    ]);
  });

  it('flags invalid numbers, addresses and restricted-signup emails', () => {
    const draft = draftFromFlags({});
    draft.magicCodeRateLimit = '0';
    draft.magicCodeExpiry = 'later';
    draft.throttleMs = 'soon';
    draft.signupMode = 'restricted';
    draft.allowedEmailsText = 'not-an-email';
    draft.webhookIpsText = '10.0.0.256';

    const problems = operatorDraftProblems(draft);

    expect(problems.magicCodeRateLimit).toBeDefined();
    expect(problems.magicCodeExpiry).toContain('1 or more');
    expect(problems.throttleMs).toBeDefined();
    expect(problems.allowedEmails).toContain('not-an-email');
    expect(problems.webhookIps).toContain('10.0.0.256');
    expect(hasDraftProblems(problems)).toBe(true);
  });

  it('accepts a valid draft', () => {
    const draft = draftFromFlags({});
    draft.signupMode = 'restricted';
    draft.allowedEmailsText = 'ops@example.com';
    draft.webhookIpsText = '10.0.0.25\n2001:db8::/32';

    expect(operatorDraftProblems(draft)).toEqual({});
  });
});

describe('advanced flags', () => {
  const flags = {
    'dashboard-signups': { mode: 'open' },
    'team-emails': [{ email: 'ops@example.com' }],
    'legacy-flag': true,
  };

  it('lists documented and undocumented flags without the typed ones', () => {
    const rows = advancedFlagRows(mockKnownOperatorFlags, flags);

    expect(rows.map((row) => row.name)).toEqual([
      'legacy-flag',
      'row-hint-threshold',
      'storage-quota-bytes',
      'team-emails',
    ]);
    expect(
      rows.find((row) => row.name === 'legacy-flag')?.type,
    ).toBeUndefined();
    expect(rows.find((row) => row.name === 'legacy-flag')?.value).toBe(true);
  });

  it('drops advanced changes that would not change the value', () => {
    expect(normaliseAdvancedChange(undefined, { action: 'reset' })).toBeNull();
    expect(
      normaliseAdvancedChange(
        { b: 2, a: 1 },
        { action: 'set', value: { a: 1, b: 2 } },
      ),
    ).toBeNull();
    expect(
      normaliseAdvancedChange(undefined, { action: 'set', value: null }),
    ).toEqual({ action: 'set', value: null });
    expect(normaliseAdvancedChange(1, { action: 'set', value: 2 })).toEqual({
      action: 'set',
      value: 2,
    });
  });

  it('compares JSON values regardless of key order', () => {
    expect(jsonValuesEqual({ a: 1, b: [1, 2] }, { b: [1, 2], a: 1 })).toBe(
      true,
    );
    expect(jsonValuesEqual([1, 2], [2, 1])).toBe(false);
  });

  it('formats stored values as JSON and leaves unset flags blank', () => {
    expect(formatFlagValue({ email: 'ops@example.com' })).toBe(
      '{"email":"ops@example.com"}',
    );
    expect(formatFlagValue(undefined)).toBeUndefined();
  });
});

describe('operator helpers', () => {
  it('splits list text into normalised addresses', () => {
    expect(parseLineList(' 10.0.0.25 \n\n10.0.0.25\n192.168.0.1')).toEqual([
      '10.0.0.25',
      '192.168.0.1',
    ]);
  });

  it('accepts addresses and CIDRs but not hostnames', () => {
    expect(isIpOrCidr('10.0.0.25')).toBe(true);
    expect(isIpOrCidr('10.0.0.0/8')).toBe(true);
    expect(isIpOrCidr('2001:db8::/32')).toBe(true);
    expect(isIpOrCidr('10.0.0.0/33')).toBe(false);
    expect(isIpOrCidr('webhooks.example.com')).toBe(false);
    expect(isIpOrCidr('10.0.0.25:8080')).toBe(false);
  });

  it('labels restore jobs and detects the active ones', () => {
    expect(
      restoreJobLabel({
        id: '3f2a9c1d-4b7e-4c2a-9d31-8e5f6a7b8c9d',
        title: 'headshot-ai copy',
      }),
    ).toBe('headshot-ai copy');
    expect(
      restoreJobLabel({
        id: 'c9d4e5f6-7a8b-4c0d-9e2f-3a4b5c6d7e8f',
        title: null,
      }),
    ).toBe('Restore c9d4e5f6');

    expect(isRestoreJobActive({ job_status: 'waiting' })).toBe(true);
    expect(isRestoreJobActive({ job_status: 'processing' })).toBe(true);
    expect(isRestoreJobActive({ job_status: 'done' })).toBe(false);
  });
});
