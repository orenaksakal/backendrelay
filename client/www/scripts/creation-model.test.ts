import test from 'node:test';
import assert from 'node:assert/strict';
import {
  checkCreation,
  creationHref,
  creationReceipt,
  creationRequest,
  creationTitle,
  organizationCandidates,
  readRecovery,
  recoveryRecord,
  targets,
  type CreationDraft,
  type Request,
} from '../components/dash/creation/model';
import { profileMeta } from '../components/dash/creation/profile';
const appId = '11111111-1111-4111-8111-111111111111',
  orgId = '22222222-2222-4222-8222-222222222222',
  token = '33333333-3333-4333-8333-333333333333';
const draft: CreationDraft = {
  kind: 'application',
  title: ' Customer portal ',
  id: appId,
  adminToken: token,
  orgId,
};
test('creation uses one prepared app ID and token; organization creation sends only its title', () => {
  const first = creationRequest(draft),
    retry = creationRequest(draft);
  assert.deepEqual(first, retry);
  assert.equal(first.path, '/dash/apps');
  assert.deepEqual(JSON.parse(first.init.body), {
    id: appId,
    admin_token: token,
    title: 'Customer portal',
    org_id: orgId,
  });
  assert.deepEqual(
    JSON.parse(
      creationRequest({ kind: 'organization', title: ' Team ' }).init.body,
    ),
    { title: 'Team' },
  );
  assert.throws(() => creationRequest({ ...draft, id: undefined }));
  assert.throws(() => creationRequest({ ...draft, adminToken: 'invalid' }));
  assert.throws(() => creationRequest({ ...draft, orgId: 'invalid' }));
  assert.throws(() => creationTitle('application', '   '));
  assert.throws(() => creationTitle('organization', 'x'.repeat(141)));
});
test('success receipts match the exact application and workspace and exclude credentials', () => {
  const receipt = creationReceipt(draft, {
    app: {
      id: appId,
      title: 'Customer portal',
      org_id: orgId,
      admin_token: token,
    },
  });
  assert.deepEqual(receipt, {
    kind: 'application',
    id: appId,
    title: 'Customer portal',
    orgId,
  });
  assert.throws(() =>
    creationReceipt(draft, {
      app: { id: orgId, title: 'Other app', org_id: orgId },
    }),
  );
  assert.throws(() =>
    creationReceipt(draft, {
      app: { id: appId, title: 'Customer portal', org_id: null },
    }),
  );
  assert.throws(() => creationReceipt(draft, {}));
  assert.throws(() =>
    creationReceipt(
      { kind: 'organization', title: 'Team' },
      { org: { id: 'new', title: 'Team' } },
    ),
  );
});
test('reload recovery is scoped metadata and never persists admin credentials', () => {
  const record = recoveryRecord(draft, [orgId], '2026-09-08T12:00:00Z');
  assert.equal(JSON.stringify(record).includes(token), false);
  assert.deepEqual(
    readRecovery(JSON.stringify({ ...record, adminToken: token })),
    record,
  );
  assert.equal(readRecovery('not json'), null);
  assert.equal(readRecovery(JSON.stringify({ ...record, id: 'bad-id' })), null);
  assert.equal(
    readRecovery(JSON.stringify({ ...record, startedAt: 'bad-date' })),
    null,
  );
  assert.equal(
    readRecovery(JSON.stringify({ ...record, previousOrgIds: [null] })),
    null,
  );
});
test('application recovery reads only its prepared ID and never retries writes', async () => {
  const requests: string[] = [];
  const request: Request = async (path, init) => {
    requests.push(path);
    assert.equal(init?.method, undefined);
    return {
      app: {
        id: appId,
        title: 'Customer portal',
        org_id: orgId,
        admin_token: token,
      },
    } as any;
  };
  const result = await checkCreation(request, recoveryRecord(draft, []));
  assert.deepEqual(requests, [`/dash/apps/${appId}`]);
  assert.equal(result.found?.recovered, true);
  assert.equal(JSON.stringify(result).includes(token), false);
});
test('recovery distinguishes missing records from denied or unavailable reads', async () => {
  const record = recoveryRecord(draft, []);
  assert.deepEqual(
    await checkCreation(async () => {
      throw { status: 404 };
    }, record),
    { missing: true },
  );
  for (const status of [401, 403, 500, 503])
    await assert.rejects(
      checkCreation(async () => {
        throw { status };
      }, record),
      (e: any) => e.status === status,
    );
});
test('organization recovery offers candidates without claiming a same-name record proves success', async () => {
  const record = recoveryRecord({ kind: 'organization', title: 'Team' }, [
    orgId,
  ]);
  const orgs = [
    { id: orgId, title: 'Team', role: 'owner', paid: false, created_at: '' },
    { id: appId, title: 'Team', role: 'owner', paid: false, created_at: '' },
    { id: token, title: 'Team', role: 'admin', paid: false, created_at: '' },
  ] as const;
  assert.deepEqual(
    organizationCandidates(record, [...orgs]).map((o) => o.id),
    [appId],
  );
  const request: Request = async (path, init) => {
    assert.equal(path, '/dash');
    assert.equal(init?.method, undefined);
    return { orgs } as any;
  };
  const result = await checkCreation(request, record);
  assert.equal(result.found, undefined);
  assert.equal(result.candidates?.[0].id, appId);
  const malformed: Request = async () => ({}) as any;
  await assert.rejects(
    checkCreation(malformed, record),
    /inventory is incomplete/,
  );
});
test('workspace selection permits collaborators and excludes app-only members', () => {
  const result = targets(
    ['owner', 'admin', 'collaborator', 'app-member'].map((role, i) => ({
      id: String(i),
      title: role,
      role,
      paid: false,
      created_at: '',
    })) as any,
  );
  assert.deepEqual(
    result.map((t) => t.id),
    ['personal', '0', '1', '2'],
  );
});
test('creation navigation retains both CLI ticket flows and actual dashboard route keys', () => {
  const url = new URL(
    creationHref(
      { kind: 'application', id: appId, title: 'Portal', orgId },
      {
        ticket: 'ticket & id',
        _cli_oauth_ticket: 'oauth / id',
        ignored: 'drop',
      },
    ),
    'https://fixture.invalid',
  );
  assert.equal(url.pathname, '/dash');
  assert.equal(url.searchParams.get('ticket'), 'ticket & id');
  assert.equal(url.searchParams.get('_cli_oauth_ticket'), 'oauth / id');
  assert.equal(url.searchParams.get('org'), orgId);
  assert.equal(url.searchParams.get('t'), 'home');
  assert.equal(url.searchParams.get('ignored'), null);
  const org = new URL(
    creationHref({ kind: 'organization', id: orgId, title: 'Team' }),
    'https://fixture.invalid',
  );
  assert.equal(org.pathname, '/dash/org');
  assert.equal(org.searchParams.get('tab'), 'members');
});
test('onboarding profile validation retains the backend metadata keys', () => {
  assert.deepEqual(profileMeta(' A colleague ', 'senior-dev'), {
    heard: 'A colleague',
    experience: 'senior-dev',
  });
  assert.throws(() => profileMeta(' ', 'senior-dev'));
  assert.throws(() => profileMeta('A colleague', 'unknown'));
});
