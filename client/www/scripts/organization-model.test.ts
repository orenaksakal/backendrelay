import test from 'node:test';
import assert from 'node:assert/strict';
import {
  assignableRoles,
  commandProblem,
  lastOwner,
  organizationTitle,
  organizationRequest,
  organizationBaseline,
  organizationReceipt,
  organizationPeople,
  organizationAppHref,
  type Organization,
} from '../components/dash/organization/model';
function org(): Organization {
  return {
    org: {
      id: 'org',
      title: 'Product team',
      role: 'owner',
      paid: false,
      created_at: '',
      updated_at: '',
    },
    apps: [],
    members: [
      { id: 'owner', email: 'owner@example.test', role: 'owner' },
      { id: 'admin', email: 'admin@example.test', role: 'admin' },
      {
        id: 'collaborator',
        email: 'collab@example.test',
        role: 'collaborator',
      },
    ],
    invites: [
      {
        id: 'inv',
        email: 'invite@example.test',
        role: 'admin',
        status: 'pending',
        sent_at: '2026-09-08T10:00:00',
        expired: false,
      },
    ],
  };
}
test('invitation permissions use hosting and actor authority, including owner invitations', () => {
  const data = org();
  assert.equal(
    commandProblem(data, 'owner@example.test', true, {
      kind: 'invite',
      email: 'new@example.test',
      role: 'owner',
    }),
    null,
  );
  assert.match(
    commandProblem(data, 'owner@example.test', false, {
      kind: 'invite',
      email: 'new@example.test',
      role: 'admin',
    })!,
    /paid/,
  );
  data.org.role = 'admin';
  assert.deepEqual(assignableRoles('admin'), ['collaborator', 'admin']);
  assert.match(
    commandProblem(data, 'admin@example.test', true, {
      kind: 'invite',
      email: 'new@example.test',
      role: 'owner',
    })!,
    /cannot invite/,
  );
  assert.match(
    commandProblem(data, 'admin@example.test', true, {
      kind: 'invite',
      email: ' INVITE@example.test ',
      role: 'admin',
    })!,
    /pending/,
  );
  assert.match(
    commandProblem(data, 'admin@example.test', true, {
      kind: 'invite',
      email: 'owner@example.test',
      role: 'admin',
    })!,
    /already a member/,
  );
  data.org.role = 'collaborator';
  assert.match(
    commandProblem(data, 'collab@example.test', true, {
      kind: 'invite',
      email: 'new@example.test',
      role: 'admin',
    })!,
    /administrators/,
  );
});
test('last-owner and target-rank protections apply to removal and demotion', () => {
  const data = org();
  assert.equal(lastOwner(data, 'owner'), true);
  assert.match(
    commandProblem(data, 'owner@example.test', true, {
      kind: 'remove',
      id: 'owner',
    })!,
    /another owner/,
  );
  assert.match(
    commandProblem(data, 'owner@example.test', true, {
      kind: 'role',
      id: 'owner',
      role: 'admin',
    })!,
    /another owner/,
  );
  data.members.push({
    id: 'second',
    email: 'second@example.test',
    role: 'owner',
  });
  assert.equal(
    commandProblem(data, 'owner@example.test', true, {
      kind: 'remove',
      id: 'owner',
    }),
    null,
  );
  data.org.role = 'admin';
  assert.match(
    commandProblem(data, 'admin@example.test', true, {
      kind: 'remove',
      id: 'owner',
    })!,
    /cannot change/,
  );
  data.org.role = 'collaborator';
  assert.equal(
    commandProblem(data, 'COLLAB@example.test', true, {
      kind: 'remove',
      id: 'collaborator',
    }),
    null,
  );
  assert.match(
    commandProblem(data, 'collab@example.test', true, {
      kind: 'remove',
      id: 'admin',
    })!,
    /cannot change/,
  );
});
test('review baselines detect role and invitation changes but ignore inventory ordering', () => {
  const data = org(),
    baseline = organizationBaseline(data);
  assert.equal(
    organizationBaseline({ ...data, members: [...data.members].reverse() }),
    baseline,
  );
  assert.notEqual(
    organizationBaseline({ ...data, org: { ...data.org, role: 'admin' } }),
    baseline,
  );
  assert.notEqual(organizationBaseline({ ...data, invites: [] }), baseline);
  assert.match(
    commandProblem({ ...data, members: [] }, 'owner@example.test', true, {
      kind: 'remove',
      id: 'owner',
    })!,
    /no longer/,
  );
  assert.match(
    commandProblem({ ...data, invites: [] }, 'owner@example.test', true, {
      kind: 'revoke',
      id: 'inv',
    })!,
    /no longer/,
  );
});
test('organization commands use scoped routes, membership IDs, normalized input and correct methods', () => {
  const remove = organizationRequest('org/id', {
    kind: 'remove',
    id: 'membership-id',
  });
  assert.equal(remove.path, '/dash/orgs/org%2Fid/members/remove');
  assert.equal(remove.init.method, 'DELETE');
  assert.deepEqual(JSON.parse(remove.init.body), { id: 'membership-id' });
  const invite = organizationRequest('org', {
    kind: 'invite',
    email: ' User@Example.test ',
    role: 'admin',
  });
  assert.deepEqual(JSON.parse(invite.init.body), {
    'invitee-email': 'user@example.test',
    role: 'admin',
  });
  assert.deepEqual(
    JSON.parse(
      organizationRequest('org', { kind: 'revoke', id: 'invite-id' }).init.body,
    ),
    { 'invite-id': 'invite-id' },
  );
  assert.equal(organizationTitle('  Studio  '), 'Studio');
  assert.equal(organizationTitle('😀'.repeat(140)).length, 280);
  assert.throws(() => organizationTitle('x'.repeat(141)));
  assert.throws(() => organizationTitle('  '));
});
test('receipts preserve real invitation IDs and only change acknowledged targets', () => {
  const data = org();
  assert.equal(
    organizationReceipt(
      data,
      { kind: 'invite', email: 'new@example.test', role: 'admin' },
      {},
    ),
    data,
  );
  assert.deepEqual(
    organizationReceipt(data, { kind: 'remove', id: 'admin' }, {}).members.map(
      (m) => m.id,
    ),
    ['owner', 'collaborator'],
  );
  assert.equal(
    organizationReceipt(data, { kind: 'revoke', id: 'inv' }, {}).invites[0]
      .status,
    'revoked',
  );
  assert.throws(() =>
    organizationReceipt(data, { kind: 'remove', id: 'admin' }, null),
  );
  assert.equal(data.members.length, 3);
});
test('people filters compose across invitation state, role and search; app links preserve context', () => {
  const data = org();
  assert.equal(
    organizationPeople(data, 'INVITE', 'pending', 'admin').length,
    1,
  );
  assert.equal(organizationPeople(data, 'INVITE', 'member', 'admin').length, 0);
  data.invites[0].expired = true;
  assert.equal(organizationPeople(data, '', 'pending', 'all').length, 0);
  assert.equal(organizationPeople(data, 'expired', 'invite', 'all').length, 1);
  const url = new URL(organizationAppHref('o & a', 'a / b'), 'http://preview');
  assert.equal(url.searchParams.get('org'), 'o & a');
  assert.equal(url.searchParams.get('app'), 'a / b');
  assert.equal(url.searchParams.get('t'), 'home');
});
test('HTTP failures from the organization transport reject instead of becoming removal receipts', async () => {
  const { jsonFetch } = await import('../lib/fetch');
  const original = globalThis.fetch;
  const data = org();
  try {
    for (const status of [400, 403, 500]) {
      globalThis.fetch = async () =>
        new Response(JSON.stringify({ message: 'Rejected removal' }), {
          status,
          headers: { 'content-type': 'application/json' },
        });
      const command = { kind: 'remove', id: 'admin' } as const;
      const operation = organizationRequest('org', command);
      let received = false;
      await assert.rejects(
        async () => {
          const response = await jsonFetch(
            `https://fixture.invalid${operation.path}`,
            operation.init,
          );
          organizationReceipt(data, command, response);
          received = true;
        },
        (error: any) => error.status === status,
      );
      assert.equal(received, false);
      assert.equal(data.members.length, 3);
    }
  } finally {
    globalThis.fetch = original;
  }
});
