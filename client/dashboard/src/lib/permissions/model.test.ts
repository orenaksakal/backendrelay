import { describe, expect, it } from 'vitest';
import {
  canEditPermissions,
  canTestPermissions,
  parseRules,
  reconstructVersion,
  ruleError,
  ruleNamespaces,
  rulesJson,
  versionsInSync,
  type RuleVersion,
} from './model';

const current = { notes: { allow: { view: 'true', create: 'false' } } };
const versions: readonly [RuleVersion, RuleVersion] = [
  {
    version: 3,
    created_at: '2026-09-08',
    edits: [[['notes', 'allow', 'view'], 'r', 'auth.id != null']],
  },
  {
    version: 2,
    created_at: '2026-09-07',
    edits: [[['notes', 'allow', 'create'], '-']],
  },
];

describe('permission rules model', () => {
  it('normalizes JSON booleans to CEL while preserving rate-limit numbers', () => {
    expect(
      parseRules(
        '{"notes":{"allow":{"view":true,"update":"auth.id != null"}},"$rateLimits":{"writes":{"limits":[{"capacity":5}]}}}',
      ),
    ).toEqual({
      notes: { allow: { view: 'true', update: 'auth.id != null' } },
      $rateLimits: { writes: { limits: [{ capacity: 5 }] } },
    });
  });

  it('rejects documents the server would refuse', () => {
    for (const input of [
      '',
      'null',
      '[]',
      'true',
      '{"notes":false}',
      '{"notes":{"allow":[]}}',
      '{"notes":{"fields":{"name":7}}}',
    ]) {
      expect(() => parseRules(input)).toThrow();
    }
  });

  it('formats the current rules and lists their namespaces', () => {
    expect(JSON.parse(rulesJson(current))).toEqual(current);
    expect(rulesJson(undefined)).toBe('{}');
    expect(
      ruleNamespaces({
        $default: {},
        $rateLimits: {},
        todos: {},
        goals: {},
      }),
    ).toEqual(['todos', 'goals']);
    expect(ruleNamespaces(null)).toEqual([]);
  });

  it('reconstructs versions without changing the live rules', () => {
    const snapshot = JSON.stringify(current);
    expect(reconstructVersion(current, 3, versions, 2)).toEqual({
      rules: { notes: { allow: { view: 'auth.id != null', create: 'false' } } },
      previous: { notes: { allow: { view: 'auth.id != null' } } },
    });
    expect(JSON.stringify(current)).toBe(snapshot);
    expect(reconstructVersion(current, 4, versions, 2)).toBeNull();
    expect(reconstructVersion(current, 3, versions, 1)).toBeNull();
    expect(
      reconstructVersion(
        current,
        3,
        [versions[0], { ...versions[1], version: 1 }],
        1,
      ),
    ).toBeNull();
    expect(
      reconstructVersion(current, 3, [versions[0], versions[0]], 3),
    ).toBeNull();
  });

  it('reconstructs root replacements and refuses malformed edits', () => {
    expect(
      reconstructVersion(
        current,
        1,
        [{ version: 1, created_at: '2026-09-08', edits: [[[], 'r', {}]] }],
        1,
      ),
    ).toEqual({ rules: current, previous: {} });
    expect(
      reconstructVersion(
        current,
        1,
        [
          {
            version: 1,
            created_at: '2026-09-08',
            edits: [[[], 'unknown', {}]],
          },
        ],
        1,
      ),
    ).toBeNull();
    expect(
      reconstructVersion(
        current,
        1,
        [{ version: 1, created_at: '2026-09-08', edits: ['bad'] }],
        1,
      ),
    ).toBeNull();
  });

  it('reports whether the edit chain covers the live rules', () => {
    expect(versionsInSync(3, versions)).toBe(true);
    expect(versionsInSync(2, versions)).toBe(false);
    expect(versionsInSync(null, versions)).toBe(false);
    expect(versionsInSync(3, [])).toBe(false);
  });

  it('keeps every server validation path in the error message', () => {
    expect(
      ruleError({
        hint: {
          errors: [
            {
              path: ['notes', 'allow', 'view'],
              message: 'Invalid expression',
            },
            { path: ['tasks'], message: 'Invalid block' },
          ],
        },
      }),
    ).toBe('notes → allow → view: Invalid expression\ntasks: Invalid block');

    expect(
      ruleError({
        body: {
          hint: {
            errors: [{ in: ['notes', 'allow', 'create'], message: 'No rule' }],
          },
        },
      }),
    ).toBe('notes → allow → create: No rule');

    expect(ruleError(new Error('Gateway unavailable'))).toBe(
      'Gateway unavailable',
    );
    expect(ruleError(undefined)).toMatch(/draft is still available/);
  });

  it('lets collaborators edit and keeps app members out', () => {
    expect(canEditPermissions('owner')).toBe(true);
    expect(canEditPermissions('admin')).toBe(true);
    expect(canEditPermissions('collaborator')).toBe(true);
    expect(canEditPermissions('app-member')).toBe(false);
  });

  it('reserves Test-as for admins and owners', () => {
    expect(canTestPermissions('owner')).toBe(true);
    expect(canTestPermissions('admin')).toBe(true);
    expect(canTestPermissions('collaborator')).toBe(false);
    expect(canTestPermissions('app-member')).toBe(false);
  });
});
