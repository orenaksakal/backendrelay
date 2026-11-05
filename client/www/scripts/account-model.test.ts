import test from 'node:test';
import assert from 'node:assert/strict';
import {
  tokenMetadata,
  tokenDate,
  tokenName,
  filterTokens,
  filterConnected,
  filterInvitations,
  accountRequest,
  inviteDestination,
  safeAccountUrl,
  connectedApps,
  accountInvitations,
} from '../components/dash/account/model';
const token = {
  id: 'token-1',
  name: 'Deploy',
  created_at: '2026-09-08T12:01:02.123',
};
const app = {
  id: 'oauth-1',
  name: 'Reporting',
  logo: null,
  homePage: 'https://reporting.example.test',
  privacyPolicyLink: null,
  tosLink: null,
};
const invite = {
  id: 'invite-1',
  type: 'app' as const,
  foreign_key: 'app-1',
  title: 'Customer portal',
  invitee_role: 'admin',
  inviter_email: 'owner@example.test',
};
test('token inventories retain metadata without retaining issued credentials', () => {
  assert.deepEqual(
    tokenMetadata({ ...token, token: 'secret-value', user_id: 'user' }),
    token,
  );
  assert.equal(
    JSON.stringify(tokenMetadata({ ...token, token: 'secret-value' })).includes(
      'secret-value',
    ),
    false,
  );
  assert.throws(() => tokenMetadata({ id: 'token-1', token: 'secret-value' }));
  assert.equal(tokenName(' Deploy script '), 'Deploy script');
  assert.throws(() => tokenName('  '));
});
test('token dates handle numeric epoch, UTC server timestamps, offsets, and invalid input', () => {
  assert.equal(tokenDate(0), '1970-01-01T00:00:00.000Z');
  assert.equal(
    tokenDate('2026-09-08T12:01:02.123'),
    '2026-09-08T12:01:02.123Z',
  );
  assert.equal(
    tokenDate('2026-09-08T06:01:02.123-06:00'),
    '2026-09-08T12:01:02.123Z',
  );
  assert.equal(tokenDate('invalid'), null);
  const old = { ...token, id: 'older', created_at: 0 };
  assert.deepEqual(
    filterTokens([old, token], '', 'newest').map((t) => t.id),
    ['token-1', 'older'],
  );
  assert.equal(filterTokens([token], 'TOKEN-1', 'name').length, 1);
});
test('account mutation routes use the exact server methods and identifiers', () => {
  assert.deepEqual(accountRequest({ kind: 'create', name: ' CI ' }), {
    path: '/dash/personal_access_tokens',
    init: { method: 'POST', body: '{"name":"CI"}' },
  });
  assert.equal(
    accountRequest({ kind: 'revoke-token', token }).init.method,
    'DELETE',
  );
  assert.equal(
    accountRequest({ kind: 'revoke-app', app }).init.body,
    '{"oauthAppId":"oauth-1"}',
  );
  assert.equal(
    accountRequest({ kind: 'accept', invite }).path,
    '/dash/invites/accept',
  );
  assert.equal(
    accountRequest({ kind: 'decline', invite }).init.body,
    '{"invite-id":"invite-1"}',
  );
});
test('connected application links reject executable and credentialed URLs while preserving visible metadata', () => {
  assert.equal(safeAccountUrl('javascript:alert(1)'), null);
  assert.equal(safeAccountUrl('https://user:pass@example.test'), null);
  assert.equal(safeAccountUrl(null), null);
  assert.equal(safeAccountUrl(app.homePage), 'https://reporting.example.test/');
  assert.equal(filterConnected([app], 'reporting.example').length, 1);
  assert.deepEqual(connectedApps([{ ...app, extra: 'ignored' }]), [app]);
  assert.throws(() => connectedApps([null]));
});
test('invitation filters preserve destination type and opened destinations use actual route keys', () => {
  const org = {
    ...invite,
    id: 'invite-2',
    type: 'org' as const,
    foreign_key: 'org-1',
  };
  assert.equal(filterInvitations([invite, org], 'owner', 'org').length, 1);
  assert.equal(filterInvitations([invite], 'admin', 'all').length, 1);
  const url = new URL(inviteDestination(invite), 'https://dash.example');
  assert.equal(url.searchParams.get('t'), 'home');
  assert.equal(url.searchParams.get('app'), 'app-1');
  assert.equal(inviteDestination(org), '/dash/org?org=org-1');
  assert.deepEqual(accountInvitations(undefined), []);
  assert.deepEqual(accountInvitations([invite]), [invite]);
  assert.throws(() => accountInvitations([{ ...invite, type: 'unknown' }]));
});
