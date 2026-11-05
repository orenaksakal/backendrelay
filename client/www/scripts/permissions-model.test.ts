import assert from 'node:assert/strict';
import test from 'node:test';
import {
  parseRules,
  reconstructVersion,
  ruleError,
  outline,
  ruleSummary,
  recoverDraft,
  type RuleVersion,
} from '../components/dash/permissions/model';

test('draft recovery preserves its saved baseline and ignores corrupt storage', () => {
  assert.deepEqual(recoverDraft('{bad', '{}'), {
    base: '{}',
    draft: '{}',
    recovered: false,
  });
  assert.deepEqual(
    recoverDraft(
      JSON.stringify({ base: '{}', draft: '{unfinished' }),
      '{"new":{}}',
    ),
    { base: '{}', draft: '{unfinished', recovered: true },
  );
  assert.equal(
    recoverDraft(JSON.stringify({ base: 1, draft: '{}' }), '{}').recovered,
    false,
  );
});

test('JSON5 rules normalize booleans to CEL while preserving rate-limit numbers', () => {
  assert.deepEqual(
    parseRules(
      "{notes:{allow:{view:true,update:'auth.id != null'}}, $rateLimits:{writes:{limits:[{capacity:5}]}}}",
    ),
    {
      notes: { allow: { view: 'true', update: 'auth.id != null' } },
      $rateLimits: { writes: { limits: [{ capacity: 5 }] } },
    },
  );
  for (const input of [
    '',
    'null',
    '[]',
    'true',
    '{notes:false}',
    '{notes:{allow:[]}}',
    '{notes:{fields:{name:7}}}',
  ])
    assert.throws(() => parseRules(input));
});
const current = { notes: { allow: { view: 'true', create: 'false' } } };
const versions: RuleVersion[] = [
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
test('version reconstruction applies reverse edits without changing the live rules', () => {
  const snapshot = JSON.stringify(current);
  assert.deepEqual(reconstructVersion(current, 3, versions, 2), {
    rules: { notes: { allow: { view: 'auth.id != null', create: 'false' } } },
    previous: { notes: { allow: { view: 'auth.id != null' } } },
  });
  assert.equal(JSON.stringify(current), snapshot);
  assert.equal(reconstructVersion(current, 4, versions, 2), null);
  assert.equal(reconstructVersion(current, 3, versions, 1), null);
  assert.equal(
    reconstructVersion(
      current,
      3,
      [versions[0], { ...versions[1], version: 1 }],
      1,
    ),
    null,
  );
  assert.equal(
    reconstructVersion(current, 3, [versions[0], versions[0]], 3),
    null,
  );
});
test('all server validation errors retain their paths and unknown failures stay visible', () => {
  assert.equal(
    ruleError({
      body: {
        hint: {
          errors: [
            { in: ['notes', 'allow', 'view'], message: 'Invalid expression' },
            { in: ['tasks'], message: 'Invalid block' },
          ],
        },
      },
    }),
    'notes → allow → view: Invalid expression\ntasks: Invalid block',
  );
  assert.match(ruleError(undefined), /draft is still available/);
});
test('history root replacements are reconstructed and malformed edits fail visibly', () => {
  assert.deepEqual(
    reconstructVersion(
      current,
      1,
      [{ version: 1, created_at: '2026-09-08', edits: [[[], 'r', {}]] }],
      1,
    ),
    { rules: current, previous: {} },
  );
  assert.equal(
    reconstructVersion(
      current,
      1,
      [
        {
          version: 1,
          created_at: '2026-09-08',
          edits: [[[], 'unknown', {}] as any],
        },
      ],
      1,
    ),
    null,
  );
});
test('outline includes schema-only tables and searches expressions without inventing effective access', () => {
  assert.deepEqual(outline(current, ['notes', 'tasks'], 'tasks'), ['tasks']);
  assert.deepEqual(outline(current, ['tasks'], 'false'), ['notes']);
  assert.deepEqual(ruleSummary(current.notes), [
    ['allow.view', 'true'],
    ['allow.create', 'false'],
  ]);
});
