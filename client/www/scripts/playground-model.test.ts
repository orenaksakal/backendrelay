import test from 'node:test';
import assert from 'node:assert/strict';
import {
  draftFingerprint,
  draftFromLink,
  executeCode,
  normalizeDraft,
  permalink,
  readExamples,
  runRules,
  safePreview,
  starterCode,
  type Draft,
  type RunOutput,
} from '../components/dash/playground/model';
const draft: Draft = {
  code: 'console.log("é 🦉");',
  perms: '{orders:{allow:{view:true}}}',
  runAsUser: 'reader@example.test',
  ipOverride: '203.0.113.7',
  originOverride: 'https://caller.example.test',
  useAppPerms: false,
};
const helpers = {
  id: () => 'fixture-id',
  tx: {},
  lookup: () => 'fixture-lookup',
};
test('playground shares Unicode code and full request context without enabling writes', () => {
  const url = permalink(
    'https://dashboard.example.test/dash?app=app-1&tab=sandbox&dangerouslyCommitTx=true',
    draft,
  );
  assert.deepEqual(
    draftFromLink(new URL(url).search, {
      ...draft,
      code: '',
      runAsUser: '',
      ipOverride: '',
      originOverride: '',
      useAppPerms: true,
    }),
    draft,
  );
  assert.equal(new URL(url).searchParams.has('dangerouslyCommitTx'), false);
  assert.equal(new URL(url).searchParams.get('app'), 'app-1');
});
test('legacy links and corrupt saves recover without losing context or admitting write flags', () => {
  assert.equal(
    draftFromLink(
      '?code=' + encodeURIComponent(btoa(JSON.stringify('console.log("é")'))),
      draft,
    ).code,
    'console.log("é")',
  );
  assert.equal(draftFromLink('?code=invalid', draft).code, draft.code);
  assert.deepEqual(readExamples('not json', draft), []);
  const restored = readExamples(
    JSON.stringify([
      null,
      { name: 'Full context', ...draft, dangerouslyCommitTx: true },
    ]),
    draft,
  )[0];
  assert.deepEqual(normalizeDraft(restored, draft), draft);
  assert.equal('dangerouslyCommitTx' in restored, false);
  assert.equal(
    draftFingerprint({ ...draft }),
    draftFingerprint(
      Object.fromEntries(Object.entries(draft).reverse()) as Draft,
    ),
  );
});
test('starter code safely quotes namespace names and never includes a transaction', () => {
  for (const namespace of ['orders', 'odd"\\\nnamespace', '日本語']) {
    const code = starterCode(namespace);
    assert.doesNotThrow(
      () => new Function('db', 'console', `return (async()=>{${code}})()`),
    );
    assert.equal(code.includes('db.transact'), false);
    assert.ok(code.includes(JSON.stringify(namespace)));
  }
});
test('custom rules normalize booleans and reject invalid containers before execution', () => {
  assert.deepEqual(runRules(draft, null), {
    orders: { allow: { view: 'true' } },
  });
  const saved = { orders: { allow: { view: 'false' } } };
  assert.equal(runRules({ ...draft, useAppPerms: true }, saved), saved);
  for (const perms of ['', '[]', 'null', 'true', '42', '{'])
    assert.throws(() => runRules({ ...draft, perms }, null));
});
test('execution uses captured request context and dry-runs transactions by default', async () => {
  const calls: any[] = [],
    output: RunOutput[] = [];
  await executeCode(
    'const data = await db.query({ orders: {} }, { ruleParams: { a: 1 }, ip: "override" }); console.log(data); return await db.transact([]);',
    {
      debugQuery: async (q, opts) => {
        calls.push(['query', q, opts]);
        return {
          result: { orders: [{ count: 0, paid: false }] },
          'check-results': [],
        };
      },
      debugTransact: async (steps, opts) => {
        calls.push(['transact', steps, opts]);
        return { 'tx-id': 42, 'all-checks-ok?': true, 'check-results': [] };
      },
    },
    {
      rules: {},
      ip: draft.ipOverride,
      origin: draft.originOverride,
      commit: false,
    },
    helpers,
    (entry) => output.push(entry),
  );
  assert.equal(calls[0][2].ip, draft.ipOverride);
  assert.deepEqual(calls[0][2].ruleParams, { a: 1 });
  assert.equal(calls[1][2].__dangerouslyCommit, false);
  assert.equal(calls[1][2].origin, draft.originOverride);
  assert.deepEqual(
    output.map((o) => o.kind),
    ['query', 'log', 'transaction', 'return'],
  );
  assert.equal(JSON.parse(output[0].text).orders[0].paid, false);
  assert.equal(JSON.parse(output[2].text).writeRequested, false);
});
test('explicit write mode reaches the debug API and stale runs cannot dispatch later transactions', async () => {
  let active = true;
  const calls: any[] = [];
  const client = {
    debugQuery: async () => {
      active = false;
      return { result: {} };
    },
    debugTransact: async (_: any, options: any) => {
      calls.push(options);
      return { 'tx-id': 1, 'all-checks-ok?': false };
    },
  };
  await executeCode(
    'await db.transact([]);',
    client,
    { rules: {}, ip: '', origin: '', commit: true },
    helpers,
    () => {},
  );
  assert.equal(calls[0].__dangerouslyCommit, true);
  await assert.rejects(
    executeCode(
      'await db.query({}); await db.transact([]);',
      client,
      { rules: {}, ip: '', origin: '', commit: true },
      helpers,
      () => {},
      () => active,
    ),
    /no longer active/,
  );
  assert.equal(calls.length, 1);
});
test('output previews tolerate circular values, BigInt, and bounded display', () => {
  const object: any = { number: 1n };
  object.self = object;
  assert.match(safePreview(object), /Circular/);
  assert.match(safePreview(object), /1n/);
  assert.match(safePreview('long'.repeat(100), 20), /Preview truncated/);
});

test('TypeScript emitted for playground compiler options executes typed declarations and top-level await', async () => {
  const ts = await import('typescript');
  const emitted = ts.transpileModule(
    'const limit: number = 3; const rows = await db.query({ projects: { $: { limit } } }); console.log(rows);',
    {
      compilerOptions: {
        target: ts.ScriptTarget.ESNext,
        module: ts.ModuleKind.ESNext,
        noEmit: false,
      },
    },
  );
  let requested: unknown;
  await executeCode(
    emitted.outputText,
    {
      debugQuery: async (query) => {
        requested = query;
        return { result: [] };
      },
      debugTransact: async () => {
        throw new Error('Unexpected transaction');
      },
    },
    { rules: {}, ip: '', origin: '', commit: false },
    helpers,
    () => {},
  );
  assert.deepEqual(requested, { projects: { $: { limit: 3 } } });
});
